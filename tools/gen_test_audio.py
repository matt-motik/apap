#!/usr/bin/env python3
"""Эталонные аудиофайлы для автотестов тракта (AM1.0 §7.1, этап С0).

Генерирует tests/data/: FLAC с MD5, FLAC с изменённым сэмплом при валидных
CRC, FLAC с нулевым MD5, FLAC с повреждённым кадром, DSF DSD64 1 кГц / 20 кГц,
DFF DSD64 1 кГц, обрезанные и повреждённые DSF/DFF (по классам CorruptKind
§2.4), MP3. Новых зависимостей Rust нет: Python (стандартная библиотека) и
системные `flac` и `lame` (их версии записываются в tests/data/README.md).

Файлы детерминированы: повторный запуск с теми же версиями `flac`/`lame`
даёт те же байты. Проверка:  python tools/gen_test_audio.py --check

Использование:
    python tools/gen_test_audio.py            # перезаписать tests/data/
    python tools/gen_test_audio.py --check    # сравнить с tests/data/, ничего не писать
"""

from __future__ import annotations

import argparse
import hashlib
import math
import shutil
import struct
import subprocess
import sys
import tempfile
import wave
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DATA = REPO / "tests" / "data"

PCM_RATE = 44_100
PCM_SECONDS = 0.5
DSD64 = 64 * 44_100
DSD_SECONDS = 0.25
DSF_BLOCK = 4096

# (имя, назначение) — попадает в README.md.
FILES: list[tuple[str, str]] = []


def note(name: str, purpose: str) -> None:
    FILES.append((name, purpose))


# --------------------------------------------------------------------- PCM / FLAC


def pcm_frames() -> list[tuple[int, int]]:
    """16-бит стерео: L — синус 1 кГц −6 дБ, R — 440 Гц −12 дБ."""
    n = int(PCM_RATE * PCM_SECONDS)
    out = []
    for i in range(n):
        left = round(16383 * math.sin(2 * math.pi * 1000 * i / PCM_RATE))
        right = round(8191 * math.sin(2 * math.pi * 440 * i / PCM_RATE))
        out.append((left, right))
    return out


def write_wav(path: Path, frames: list[tuple[int, int]]) -> None:
    with wave.open(str(path), "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(PCM_RATE)
        w.writeframes(b"".join(struct.pack("<hh", l, r) for l, r in frames))


def flac_encode(wav: Path, out: Path) -> None:
    subprocess.run(
        ["flac", "--silent", "--force", "-8", "--no-padding", "-o", str(out), str(wav)],
        check=True,
    )


def audio_start(flac: bytes) -> int:
    """Смещение первого аудиокадра: после всех блоков метаданных."""
    if flac[:4] != b"fLaC":
        raise ValueError("не FLAC")
    pos = 4
    while True:
        header = flac[pos]
        length = int.from_bytes(flac[pos + 1 : pos + 4], "big")
        pos += 4 + length
        if header & 0x80:
            return pos


# STREAMINFO — первый блок: заголовок блока 4 байта, MD5 — последние 16 из 34.
MD5_OFFSET = 4 + 4 + 18


def gen_flac(tmp: Path, out: Path) -> None:
    frames = pcm_frames()
    wav = tmp / "src.wav"
    write_wav(wav, frames)
    good = out / "flac_16_44k1_md5.flac"
    flac_encode(wav, good)
    note(good.name, "FLAC 16 бит / 44,1 кГц, стерео, 0,5 с; MD5 в STREAMINFO совпадает с аудио")
    good_bytes = good.read_bytes()
    md5 = good_bytes[MD5_OFFSET : MD5_OFFSET + 16]

    # Один сэмпл изменён: кадры закодированы заново (CRC валидны), MD5 — от исходного аудио.
    changed = list(frames)
    i = len(changed) // 2
    changed[i] = (changed[i][0] ^ 1, changed[i][1])
    wav2 = tmp / "changed.wav"
    write_wav(wav2, changed)
    mismatch = out / "flac_md5_mismatch.flac"
    flac_encode(wav2, mismatch)
    data = bytearray(mismatch.read_bytes())
    data[MD5_OFFSET : MD5_OFFSET + 16] = md5
    mismatch.write_bytes(bytes(data))
    note(mismatch.name, f"как {good.name}, но сэмпл L кадра {i} изменён (CRC кадров валидны), MD5 — исходный → «MD5 не совпал»")

    zero = out / "flac_md5_zero.flac"
    data = bytearray(good_bytes)
    data[MD5_OFFSET : MD5_OFFSET + 16] = bytes(16)
    zero.write_bytes(bytes(data))
    note(zero.name, f"как {good.name}, MD5 в STREAMINFO нулевой → проверка MD5 «Absent»")

    corrupt = out / "flac_corrupt_frame.flac"
    data = bytearray(good_bytes)
    start = audio_start(good_bytes)
    pos = start + (len(data) - start) // 2
    data[pos] ^= 0xFF
    corrupt.write_bytes(bytes(data))
    note(corrupt.name, f"как {good.name}, байт аудиоданных по смещению {pos} инвертирован (CRC кадра не сходится)")


def gen_mp3(tmp: Path, out: Path) -> None:
    wav = tmp / "src.wav"
    mp3 = out / "mp3_44k1_stereo.mp3"
    subprocess.run(
        ["lame", "--quiet", "-b", "192", "--noreplaygain", str(wav), str(mp3)],
        check=True,
    )
    note(mp3.name, "MP3 192 кбит/с, 44,1 кГц, стерео (то же аудио, что FLAC) — lossy-источник")


# --------------------------------------------------------------------- DSD


def sigma_delta(freq: float, amplitude: float, n: int) -> list[int]:
    """1-битный поток (0/1) модулятором 2-го порядка; детерминирован."""
    i1 = i2 = 0.0
    y = 1.0
    out = []
    step = 2 * math.pi * freq / DSD64
    for k in range(n):
        x = amplitude * math.sin(step * k)
        i1 += x - y
        i2 += i1 - y
        y = 1.0 if i2 >= 0 else -1.0
        out.append(1 if y > 0 else 0)
    return out


def pack_bits(bits: list[int], lsb_first: bool) -> bytes:
    out = bytearray(len(bits) // 8)
    for j in range(len(out)):
        b = 0
        for k in range(8):
            if bits[8 * j + k]:
                b |= (1 << k) if lsb_first else (0x80 >> k)
        out[j] = b
    return bytes(out)


def dsd_channels(freq: float) -> list[list[int]]:
    n = int(DSD64 * DSD_SECONDS) // 8 * 8
    left = sigma_delta(freq, 0.5, n)
    right = sigma_delta(freq, 0.25, n)
    return [left, right]


def dsf_bytes(chans: list[list[int]], *, channels: int | None = None, block: int = DSF_BLOCK,
              data_size: int | None = None, meta_ptr: int = 0, magic: bytes = b"DSD ") -> bytes:
    per_ch = [pack_bits(c, lsb_first=True) for c in chans]
    samples = len(chans[0])
    blocks = -(-len(per_ch[0]) // DSF_BLOCK)
    body = bytearray()
    for b in range(blocks):
        for ch in per_ch:
            chunk = ch[b * DSF_BLOCK : (b + 1) * DSF_BLOCK]
            body += chunk + bytes(DSF_BLOCK - len(chunk))
    nch = len(chans) if channels is None else channels
    fmt = b"fmt " + struct.pack("<QIIIIIIQII", 52, 1, 0, 2, nch, DSD64, 1, samples, block, 0)
    data = b"data" + struct.pack("<Q", 12 + len(body) if data_size is None else data_size) + bytes(body)
    total = 28 + len(fmt) + len(data)
    head = magic + struct.pack("<QQQ", 28, total, meta_ptr)
    return head + fmt + data


def dff_bytes(chans: list[list[int]], *, channels: int | None = None, dsd_size: int | None = None,
              magic: bytes = b"FRM8") -> bytes:
    per_ch = [pack_bits(c, lsb_first=False) for c in chans]
    body = bytes(b for frame in zip(*per_ch) for b in frame)
    nch = len(chans) if channels is None else channels
    ids = [b"SLFT", b"SRGT"][:nch]
    fs = b"FS  " + struct.pack(">QI", 4, DSD64)
    chnl_data = struct.pack(">H", nch) + b"".join(ids)
    chnl = b"CHNL" + struct.pack(">Q", len(chnl_data)) + chnl_data
    name = b"not compressed"
    cmpr_data = b"DSD " + bytes([len(name)]) + name
    cmpr_data += bytes(len(cmpr_data) % 2)
    cmpr = b"CMPR" + struct.pack(">Q", len(cmpr_data)) + cmpr_data
    prop_data = b"SND " + fs + chnl + cmpr
    prop = b"PROP" + struct.pack(">Q", len(prop_data)) + prop_data
    fver = b"FVER" + struct.pack(">QI", 4, 0x01050000)
    dsd = b"DSD " + struct.pack(">Q", len(body) if dsd_size is None else dsd_size) + body
    form = b"DSD " + fver + prop + dsd
    return magic + struct.pack(">Q", len(form)) + form


def gen_dsd(out: Path) -> None:
    c1k = dsd_channels(1000.0)
    c20k = dsd_channels(20_000.0)

    def put(name: str, data: bytes, purpose: str) -> None:
        (out / name).write_bytes(data)
        note(name, purpose)

    good = dsf_bytes(c1k)
    put("dsf_dsd64_1k.dsf", good, "DSF DSD64, стерео, 0,25 с; синус 1 кГц (L −6 дБ, R −12 дБ), Σ∆ 2-го порядка")
    put("dsf_dsd64_20k.dsf", dsf_bytes(c20k), "DSF DSD64, стерео, 0,25 с; синус 20 кГц (L −6 дБ, R −12 дБ)")
    dff = dff_bytes(c1k)
    put("dff_dsd64_1k.dff", dff, "DFF (DSDIFF 1.5) DSD64, стерео, несжатый; те же биты, что dsf_dsd64_1k.dsf")

    # Повреждённые — по одному на класс CorruptKind (§2.4, §6.29).
    put("dsf_truncated.dsf", good[: len(good) // 2], "Truncated: DSF обрезан посередине данных")
    put("dsf_bad_header.dsf", dsf_bytes(c1k, magic=b"DSX "), "BadHeader: сигнатура «DSX » вместо «DSD »")
    put("dsf_zero_channels.dsf", dsf_bytes(c1k, channels=0), "ZeroChannels: число каналов 0")
    put("dsf_block_size_out_of_range.dsf", dsf_bytes(c1k, block=0x8000_0000), "BlockSizeOutOfRange: размер блока 2³¹")
    put("dsf_chunk_size_overflow.dsf", dsf_bytes(c1k, data_size=0xFFFF_FFFF_FFFF_FFFF), "ChunkSizeOverflow: размер чанка data = u64::MAX")
    put("dsf_offset_beyond_eof.dsf", dsf_bytes(c1k, meta_ptr=0x7FFF_FFFF_FFFF), "OffsetBeyondEof: указатель метаданных за концом файла")
    put("dff_truncated.dff", dff[: len(dff) // 2], "Truncated: DFF обрезан посередине данных")
    put("dff_bad_header.dff", dff_bytes(c1k, magic=b"FRM9"), "BadHeader: сигнатура «FRM9» вместо «FRM8»")
    put("dff_zero_channels.dff", dff_bytes(c1k, channels=0), "ZeroChannels: CHNL с нулём каналов")
    put("dff_chunk_size_overflow.dff", dff_bytes(c1k, dsd_size=0xFFFF_FFFF_FFFF_FFFF), "ChunkSizeOverflow: размер чанка DSD = u64::MAX")


# --------------------------------------------------------------------- main


def tool_version(cmd: list[str]) -> str:
    return subprocess.run(cmd, check=True, capture_output=True, text=True).stdout.splitlines()[0].strip()


def readme(out: Path) -> None:
    lines = [
        "# Эталонные аудиофайлы тестов (AM1.0 §7.1)",
        "",
        "Сгенерированы `tools/gen_test_audio.py` — не редактировать вручную.",
        f"Инструменты: `{tool_version(['flac', '--version'])}`, `{tool_version(['lame', '--version'])}`.",
        "Проверка, что файлы соответствуют скрипту: `python tools/gen_test_audio.py --check`.",
        "",
        "| Файл | Назначение | SHA-256 (первые 16) |",
        "|---|---|---|",
    ]
    for name, purpose in FILES:
        digest = hashlib.sha256((out / name).read_bytes()).hexdigest()[:16]
        lines.append(f"| `{name}` | {purpose} | `{digest}` |")
    (out / "README.md").write_text("\n".join(lines) + "\n", encoding="utf-8")


def generate(out: Path) -> None:
    for tool in ("flac", "lame"):
        if shutil.which(tool) is None:
            sys.exit(f"нужен системный `{tool}`")
    FILES.clear()
    out.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as t:
        tmp = Path(t)
        gen_flac(tmp, out)
        gen_mp3(tmp, out)
    gen_dsd(out)
    readme(out)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--check", action="store_true", help="сравнить с tests/data/ без записи")
    args = ap.parse_args()
    if not args.check:
        generate(DATA)
        print(f"записано {len(FILES)} файлов в {DATA.relative_to(REPO)}")
        return 0
    with tempfile.TemporaryDirectory() as t:
        fresh = Path(t)
        generate(fresh)
        bad = [p.name for p in sorted(fresh.iterdir())
               if not (DATA / p.name).exists() or (DATA / p.name).read_bytes() != p.read_bytes()]
    if bad:
        print("отличаются от скрипта:", ", ".join(bad))
        return 1
    print(f"OK — {len(FILES)} файлов совпадают со скриптом")
    return 0


if __name__ == "__main__":
    sys.exit(main())
