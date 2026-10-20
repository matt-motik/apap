//! Определение процесса, который держит PCM `hw:` в обход резервирования
//! (ТЗ-122, §6.6). Вызывается один раз движком после финального `EBUSY` при
//! открытии PCM (§6.6: `busy_owner(card, d)` в ветке `Err(EBUSY)` после
//! истечения дедлайна) — не в аудио-потоке, поэтому обычное файловое I/O
//! допустимо. Прав администратора не требует: читает только `/proc`.
//!
//! Работает на Linux (модуль собирается только под `target_os = "linux"`):
//! на Windows и macOS захвата устройства нет (ТЗ-110), владелец там не
//! определяется.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::fs;
use std::path::Path;

use crate::audio::error::BusyOwner;

/// Определяет владельца PCM `hw:CARD=<card>,DEV=<device>` по `/proc`
/// (ТЗ-122, §6.6). Вызывается из `owner_of` (`src/audio/backend/alsa.rs`),
/// которую `open_retry` зовёт на ветке финального `EBUSY` — для первого
/// захвата (`acquire`) и переоткрытия при `reconfigure` (`reopen_pcm`).
pub(crate) fn busy_owner(card: u32, device: u32) -> Option<BusyOwner> {
    busy_owner_in(Path::new("/proc"), card, device)
}

/// Та же логика с подменным корнем `/proc` — для тестов (ТЗ-122, §6.6).
fn busy_owner_in(proc_root: &Path, card: u32, device: u32) -> Option<BusyOwner> {
    let pcm_dir = proc_root.join(format!("asound/card{card}/pcm{device}p"));
    let mut sub_entries: Vec<_> = fs::read_dir(&pcm_dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("sub"))
        .collect();
    sub_entries.sort_by_key(|entry| entry.file_name());

    for entry in sub_entries {
        let status_path = entry.path().join("status");
        let Ok(status) = fs::read_to_string(&status_path) else {
            continue;
        };
        let Some(pid) = parse_owner_pid(&status) else {
            continue;
        };
        let comm_path = proc_root.join(pid.to_string()).join("comm");
        let Ok(comm) = fs::read_to_string(&comm_path) else {
            continue;
        };
        let name = comm.trim_end_matches('\n').to_string();
        if name.is_empty() {
            continue;
        }
        return Some(BusyOwner { name, pid });
    }
    None
}

/// Разбор строки `owner_pid   : <n>` из `status` субпотока PCM (ТЗ-122,
/// §6.6). Закрытый субпоток: файл содержит `closed` без `owner_pid`.
fn parse_owner_pid(status: &str) -> Option<u32> {
    for line in status.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim() == "owner_pid" {
            return value.trim().parse().ok();
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("mp_busy_owner_{tag}_{}", std::process::id()))
    }

    fn write_sub_status(root: &Path, card: u32, device: u32, sub: &str, status: &str) {
        let dir = root.join(format!("asound/card{card}/pcm{device}p/{sub}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("status"), status).unwrap();
    }

    fn write_comm(root: &Path, pid: u32, comm: &str) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("comm"), comm).unwrap();
    }

    #[test]
    fn busy_owner_reads_proc_status() {
        let root = temp_root("reads_status");
        write_sub_status(&root, 1, 0, "sub0", "state: RUNNING\nowner_pid   : 4242\ntrigger_time: 0\n");
        write_comm(&root, 4242, "aplay\n");

        let owner = busy_owner_in(&root, 1, 0);
        assert_eq!(owner, Some(BusyOwner { name: "aplay".to_string(), pid: 4242 }));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn busy_owner_closed_status_is_none() {
        let root = temp_root("closed");
        write_sub_status(&root, 1, 0, "sub0", "closed\n");

        assert_eq!(busy_owner_in(&root, 1, 0), None);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn busy_owner_without_comm_is_none() {
        let root = temp_root("no_comm");
        write_sub_status(&root, 1, 0, "sub0", "state: RUNNING\nowner_pid   : 4242\n");

        assert_eq!(busy_owner_in(&root, 1, 0), None);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn busy_owner_skips_closed_subs_before_owner() {
        let root = temp_root("multi_sub");
        write_sub_status(&root, 1, 0, "sub0", "closed\n");
        write_sub_status(&root, 1, 0, "sub1", "state: RUNNING\nowner_pid   : 99\n");
        write_comm(&root, 99, "jackd\n");

        let owner = busy_owner_in(&root, 1, 0);
        assert_eq!(owner, Some(BusyOwner { name: "jackd".to_string(), pid: 99 }));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn parse_owner_pid_cases() {
        assert_eq!(parse_owner_pid("state: RUNNING\nowner_pid   : 17650\ntrigger_time: 0\n"), Some(17650));
        assert_eq!(parse_owner_pid("closed\n"), None);
        assert_eq!(parse_owner_pid("owner_pid   : garbage\n"), None);
        assert_eq!(parse_owner_pid(""), None);
    }
}
