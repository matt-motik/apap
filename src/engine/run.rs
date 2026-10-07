//! Цикл потока `apap-engine` (§6.1, ADR-01, ADR-02, ADR-20, ТЗ-103). Мост С3:
//! `Engine` строится внутри самого потока (`Player` не обязан быть `Send`,
//! §2.10) и держит легаси `Player` целиком вместо `SignalPath`/сессии (С4/С5).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::audio::backend::catalog::DeviceCatalog;
use crate::audio::error::{EngineFault, FileError, OpenError};
use crate::audio::player::Player;
use crate::engine::deps::EngineDeps;
use crate::engine::messages::{EngineCmd, EngineEvent, SkipReason, TransportState};

/// Цикл обработки команд (мост С3): владеет `Player` целиком, пока
/// `SignalPath`/`BadgeState`/`DeviceCatalog.hw` не появились (С4…С6).
struct Engine {
    player: Player,
    deps: EngineDeps,
    /// `req_gen` и путь последнего успешно открытого трека — для `SetDevice`
    /// (переоткрытие на новом устройстве) и для `req_gen` в `OpenFailed`.
    current: Option<(u64, Arc<Path>)>,
    /// Кэш перечисления Shared-устройств (ADR-16, §2.3); пополняется по
    /// `RefreshDevices`. Событие `Devices` и разбор ошибок — шаг 20.
    catalog: DeviceCatalog,
}

impl Engine {
    fn new(deps: EngineDeps) -> Engine {
        let mut player = Player::new();
        player.set_spawner(Arc::clone(&deps.spawner));
        Engine { player, deps, current: None, catalog: DeviceCatalog::default() }
    }

    /// Цикл движка (§6.1): команда — сразу `handle`, тайм-аут 20 мс —
    /// следующая итерация. Опрос сессии (`Position`/`Transport`/`Ended`,
    /// §6.1 п. 1–5) добавляется в шаге 18; здесь взять движку ещё нечего —
    /// сессии и `SignalPath` нет (мост С3).
    fn run(mut self, rx: Receiver<EngineCmd>) {
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(cmd) => {
                    if self.handle(cmd) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                // UI исчез без Shutdown (§6.1): реагируем так же, как на Shutdown.
                Err(RecvTimeoutError::Disconnected) => {
                    self.handle(EngineCmd::Shutdown);
                    break;
                }
            }
        }
    }

    /// Обрабатывает одну команду. Возвращает `true`, если цикл должен
    /// завершиться (`Shutdown`).
    fn handle(&mut self, cmd: EngineCmd) -> bool {
        match cmd {
            EngineCmd::Open { req_gen, path, start_secs, autoplay } => {
                match self.player.open(&path) {
                    Ok(info) => {
                        if start_secs > 0.0 {
                            self.player.seek(start_secs);
                        }
                        if autoplay {
                            self.player.play();
                        }
                        self.current = Some((req_gen, Arc::clone(&path)));
                        let stream = self.player.stream_desc().cloned();
                        self.deps.events.emit(EngineEvent::Opened { req_gen, info, stream });
                        self.emit_transport();
                    }
                    Err(msg) => {
                        // Мост С3: ошибка открытия ещё не классифицирована
                        // (`FileError` точнее — §6.18 шаг 19); пока заворачиваем
                        // текст легаси-ошибки в `Unsupported`.
                        self.deps.events.emit(EngineEvent::Skipped {
                            req_gen,
                            path,
                            reason: SkipReason::File(FileError::Unsupported { codec: msg }),
                        });
                    }
                }
            }
            EngineCmd::Play => {
                self.player.play();
                self.emit_transport();
            }
            EngineCmd::Pause => {
                // `Player::toggle` переключает play/pause; команда `Pause`
                // не должна запускать воспроизведение, если оно уже на паузе
                // или остановлено, поэтому переключаем только из «играет».
                if self.player.is_playing() {
                    self.player.toggle();
                }
                self.emit_transport();
            }
            EngineCmd::Stop => {
                self.player.stop();
                self.deps.events.emit(EngineEvent::Transport { state: TransportState::Stopped });
            }
            EngineCmd::Seek { secs } => {
                self.player.seek(secs);
            }
            EngineCmd::SetVolume(v) => {
                self.player.set_volume(v);
            }
            EngineCmd::SetMuted(m) => {
                self.player.set_muted(m);
            }
            EngineCmd::SetDevice { id } => {
                let (req_gen, path) = match &self.current {
                    Some((gen, path)) => (*gen, Some(path.as_ref())),
                    None => (0, None),
                };
                let (_, pos, _) = self.player.snapshot();
                match self.player.set_device(id, path, pos) {
                    Ok(()) => self.emit_transport(),
                    Err(_msg) => {
                        // Мост С3: `set_device` возвращает текст ошибки, а не
                        // классифицированный `CaptureFailure` (§6.6 — будущий
                        // шаг); ближайший по смыслу вариант — потеря устройства.
                        self.deps
                            .events
                            .emit(EngineEvent::OpenFailed { req_gen, err: OpenError::DeviceLost });
                    }
                }
            }
            EngineCmd::RefreshDevices => {
                // Мост С3: событие `Devices` и разбор `BackendError` — шаг 20;
                // здесь каталог просто обновляется, если устройства сменились.
                let _ = self.catalog.refresh(self.deps.shared.as_mut());
            }
            EngineCmd::SetVizTap(on) => {
                self.player.set_viz_tap_active(on);
            }
            EngineCmd::AttachVizTap(producer) => {
                self.player.set_viz_tap(producer);
            }
            EngineCmd::SetLegacyAudio(audio) => {
                // Применение старых настроек звука в памяти движка без I/O
                // (ТЗ-134, И-Р24); заменяется `SetModeSettings`/`SetActiveMode` в С4.
                self.player.set_exclusive_mode(audio.exclusive_mode);
                self.player.set_fallback_policy(audio.fallback_policy);
                self.player.set_dsd_mode(audio.dsd_mode);
                self.player.set_resampler_mode(audio.resampler_mode);
                self.player.set_resampler_algorithm(audio.resampler_algorithm);
                self.player.set_fixed_rate(audio.fixed_rate);
                self.player.set_prefer_family(audio.prefer_family);
                self.player.set_fallback_rate(audio.fallback_rate);
                self.player.set_ring_buffer_ms(audio.ring_buffer_ms);
                self.player.set_bit_perfect(audio.bit_perfect);
                self.player.set_dither(audio.dither);
            }
            EngineCmd::Shutdown => {
                self.player.release_engine();
                self.deps.events.emit(EngineEvent::ShutdownComplete);
                return true;
            }
        }
        false
    }

    /// `Transport` по текущему состоянию `Player` (мост С3): `Playing`, если
    /// идёт воспроизведение; `Paused`, если трек загружен, но не играет;
    /// иначе `Stopped`. `Stop` формирует `Transport::Stopped` напрямую — у
    /// легаси `Player` декодер остаётся загруженным и после `stop()`.
    fn emit_transport(&self) {
        let state = if self.player.is_playing() {
            TransportState::Playing
        } else if self.player.has_decoder() {
            TransportState::Paused
        } else {
            TransportState::Stopped
        };
        self.deps.events.emit(EngineEvent::Transport { state });
    }
}

/// Handle движка в UI (§2.8, ADR-01): хранит неблокирующий `Sender` и
/// `JoinHandle` для выхода (ТЗ-45).
pub struct EngineHandle {
    tx: mpsc::Sender<EngineCmd>,
    join: Option<JoinHandle<()>>,
}

impl EngineHandle {
    /// Запускает поток `apap-engine` через `deps.spawner` (ТЗ-88, ADR-20).
    pub fn spawn(deps: EngineDeps) -> Result<EngineHandle, EngineFault> {
        let spawner = Arc::clone(&deps.spawner);
        let (tx, rx) = mpsc::channel();
        let join = spawner.spawn(
            "apap-engine",
            Box::new(move || {
                let engine = Engine::new(deps);
                engine.run(rx);
            }),
        )?;
        Ok(EngineHandle { tx, join: Some(join) })
    }

    /// Неблокирующая отправка команды (ADR-01). `false`, если движок уже
    /// завершился (получатель отключён).
    pub fn send(&self, cmd: EngineCmd) -> bool {
        self.tx.send(cmd).is_ok()
    }

    /// Дожидается завершения потока движка. Берёт `JoinHandle` из `self`,
    /// чтобы `Drop` не пытался join повторно.
    pub fn join(mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(EngineCmd::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::audio::backend::catalog::FakeDeviceWatcher;
    use crate::audio::backend::shared::FakeSharedBackend;
    use crate::audio::format::{ChannelLayout, Codec, Container, SampleRate, SourceFormat, SourceKind};
    use crate::audio::clock::MonotonicClock;
    use crate::engine::sink::VecSink;
    use crate::engine::source::FakeSource;
    use crate::engine::spawner::{FailingSpawner, StdSpawner};
    use crate::platform::fs::{FsPersistStore, MemStore};
    use std::path::PathBuf;

    fn pcm_format() -> SourceFormat {
        SourceFormat {
            container: Container::Flac,
            codec: Codec::Flac,
            lossy: false,
            rate: SampleRate::new(44_100).expect("rate"),
            layout: ChannelLayout::stereo(),
            kind: SourceKind::Pcm { bits: crate::audio::format::BitDepth::new(16).expect("bits") },
        }
    }

    fn fake_deps(events: Box<dyn crate::engine::sink::EventSink>, spawner: Arc<dyn crate::engine::spawner::ThreadSpawner>) -> EngineDeps {
        let mem = MemStore::new();
        EngineDeps {
            shared: Box::new(FakeSharedBackend::with_devices(Vec::new())),
            watcher: Box::new(FakeDeviceWatcher::new()),
            sources: Arc::new(FakeSource::with_format(pcm_format())),
            clock: Box::new(MonotonicClock::new()),
            store: Box::new(FsPersistStore::new(Arc::new(mem.clone()), Box::new(mem.clone()), PathBuf::from("/cfg"))),
            events,
            spawner,
        }
    }

    #[test]
    fn engine_spawn_and_shutdown() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink.clone()), Arc::new(StdSpawner));

        let handle = EngineHandle::spawn(deps).expect("spawn ok");
        assert!(handle.send(EngineCmd::SetVolume(0.5)));
        assert!(handle.send(EngineCmd::Shutdown));
        handle.join();

        let events = sink.take();
        assert!(events.iter().any(|e| matches!(e, EngineEvent::ShutdownComplete)));
    }

    #[test]
    fn engine_spawn_failure_reported() {
        let sink = VecSink::new();
        let deps = fake_deps(Box::new(sink), Arc::new(FailingSpawner::new(1)));

        match EngineHandle::spawn(deps) {
            Err(EngineFault::SpawnFailed { what: "apap-engine" }) => {}
            Err(other) => panic!("expected SpawnFailed, got {other:?}"),
            Ok(_) => panic!("expected spawn to fail"),
        }
    }
}
