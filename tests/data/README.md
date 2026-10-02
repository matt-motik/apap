# Эталонные аудиофайлы тестов (AM1.0 §7.1)

Сгенерированы `tools/gen_test_audio.py` — не редактировать вручную.
Инструменты: `flac 1.5.0`, `LAME 64bits version 4.0 (https://lame.sourceforge.io)`.
Проверка, что файлы соответствуют скрипту: `python tools/gen_test_audio.py --check`.

| Файл | Назначение | SHA-256 (первые 16) |
|---|---|---|
| `flac_16_44k1_md5.flac` | FLAC 16 бит / 44,1 кГц, стерео, 0,5 с; MD5 в STREAMINFO совпадает с аудио | `16b77a0c59b6f6bf` |
| `flac_md5_mismatch.flac` | как flac_16_44k1_md5.flac, но сэмпл L кадра 11025 изменён (CRC кадров валидны), MD5 — исходный → «MD5 не совпал» | `3b4971952c3302b1` |
| `flac_md5_zero.flac` | как flac_16_44k1_md5.flac, MD5 в STREAMINFO нулевой → проверка MD5 «Absent» | `fb383672e1a27205` |
| `flac_corrupt_frame.flac` | как flac_16_44k1_md5.flac, байт аудиоданных по смещению 8666 инвертирован (CRC кадра не сходится) | `c94a2de799813eaa` |
| `mp3_44k1_stereo.mp3` | MP3 192 кбит/с, 44,1 кГц, стерео (то же аудио, что FLAC) — lossy-источник | `1a9ebbaae5a880d8` |
| `dsf_dsd64_1k.dsf` | DSF DSD64, стерео, 0,25 с; синус 1 кГц (L −6 дБ, R −12 дБ), Σ∆ 2-го порядка | `59a57dde5e1d4b79` |
| `dsf_dsd64_20k.dsf` | DSF DSD64, стерео, 0,25 с; синус 20 кГц (L −6 дБ, R −12 дБ) | `b75ac604e77759ca` |
| `dff_dsd64_1k.dff` | DFF (DSDIFF 1.5) DSD64, стерео, несжатый; те же биты, что dsf_dsd64_1k.dsf | `bd587bcb7cc70592` |
| `dsf_truncated.dsf` | Truncated: DSF обрезан посередине данных | `10011a7c504cab79` |
| `dsf_bad_header.dsf` | BadHeader: сигнатура «DSX » вместо «DSD » | `b2952b17d9ec20fb` |
| `dsf_zero_channels.dsf` | ZeroChannels: число каналов 0 | `8923a7e5e14705c4` |
| `dsf_block_size_out_of_range.dsf` | BlockSizeOutOfRange: размер блока 2³¹ | `f73c85825fa1b91f` |
| `dsf_chunk_size_overflow.dsf` | ChunkSizeOverflow: размер чанка data = u64::MAX | `038b36f06f38e888` |
| `dsf_offset_beyond_eof.dsf` | OffsetBeyondEof: указатель метаданных за концом файла | `5692c0e6fa298dfc` |
| `dff_truncated.dff` | Truncated: DFF обрезан посередине данных | `9c7989d8c3b79e05` |
| `dff_bad_header.dff` | BadHeader: сигнатура «FRM9» вместо «FRM8» | `ae5ea72296c39133` |
| `dff_zero_channels.dff` | ZeroChannels: CHNL с нулём каналов | `13fa16bdedfee480` |
| `dff_chunk_size_overflow.dff` | ChunkSizeOverflow: размер чанка DSD = u64::MAX | `d590160c8c2cf6e0` |
