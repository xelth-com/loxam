# loxam

[![Rust](https://img.shields.io/badge/Rust-2021-orange?logo=rust)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-windows%20%7C%20linux%20%7C%20macos-lightgrey)](#)

*[English version below](#english)*

---

**`loxam`** — инструмент для восстановления ZIP-архивов, повреждённых при передаче по FTP в текстовом режиме (ASCII mode). В таком режиме FTP-клиент применяет преобразование окончаний строк к бинарному содержимому, превращая каждый байт `0x0A` (`\n`) в последовательность `0x0D 0x0A` (`\r\n`). Для архива размером в сотни мегабайт это означает вставку десятков тысяч, а то и миллионов «лишних» байт — и полное разрушение структуры.

Стандартные утилиты (`unzip`, `7z`, `zip -FF`) в этом случае бессильны: они видят рассыпавшиеся CRC, поломанные заголовки и сдвинутые смещения — и аварийно завершаются.

`loxam` умеет откатить это преобразование, восстановив исходный архив побайтно.

## Описание

Утилита решает одну конкретную, но болезненную задачу: **удалить только те `\r`, которые FTP вставил перед каждым `\n`, не тронув «естественные» последовательности `\r\n`, которые случайно встречаются в сжатых данных.**

Она работает с ZIP-архивами любого размера — от нескольких килобайт до многогигабайтных, — используя zero-copy mmap-ввод, параллельное восстановление файлов через `rayon` и поддержку ZIP64. Для монолитных payload'ов (один большой сжатый файл с тысячами натуральных CRLF) применяется **Stateful Beam Search** поверх клонируемого состояния декомпрессора `miniz_oxide`.

## Проблема

FTP в текстовом режиме рассчитан на передачу ASCII-файлов между системами с разными соглашениями об окончаниях строк (Unix: `\n`, Windows: `\r\n`, классический Mac: `\r`). При передаче от Unix-сервера к Windows-клиенту FTP применяет правило:

```
каждый байт 0x0A  →  последовательность 0x0D 0x0A
```

Для текста это безобидно. Для бинарного ZIP это катастрофа:

1. **Разрушаются CRC32.** Любой файл внутри архива, в сжатых данных которого встречался байт `0x0A`, получает лишние `0x0D` и перестаёт проходить проверку целостности.
2. **Сдвигаются смещения.** Central Directory хранит абсолютные офсеты локальных заголовков. Вставка даже одного байта сдвигает все последующие данные, и каталог указывает «в никуда».
3. **Ломаются Deflate-потоки.** Сжатие использует битовый поток с неравномерной упаковкой — вставка лишнего байта ломает Huffman-декодирование в первом же блоке.
4. **Ложные срабатывания.** Некоторые `\r\n` в архиве **натуральные** (случайные комбинации в сжатых данных, в среднем ~1 на 64 КБ) — их удалять **нельзя**. Наивный подход «убрать все `\r` перед `\n`» ломает примерно столько же данных, сколько чинит.

Типовая цифра для архива 380 МБ: ~1 500 000 вставленных `\r` нужно удалить, ~6 000 натуральных `\r\n` нужно сохранить. Полный перебор комбинаций — `2^6000` вариантов — невозможен.

## Как это работает

`loxam` комбинирует три взаимодополняющих алгоритма, применяемых послойно.

### 1. Header-First Stabilization

Вокруг каждой сигнатуры локального заголовка файла (`PK\x03\x04`) лежат 16 критических байт: версия, флаги, метод сжатия, CRC32, размеры, длины имени и extra-поля. Если хотя бы один из них пострадал от `\n→\r\n`, весь следующий Deflate-поток невозможно даже начать декодировать.

Решение:

- Поиск всех сигнатур `PK\x03\x04` выполняется через `memchr` за один проход по mmap-буферу.
- Для каждой сигнатуры перебираются все комбинации присутствия/отсутствия `\r` в её 16-байтовой шапке (малая степень двойки, порядка сотен вариантов).
- Каждая комбинация валидируется по известным инвариантам: поле версии ∈ {10, 20, 45}, метод сжатия ∈ {0, 8}, `compressed_size` согласован с длиной до следующей сигнатуры, и т. д.
- Как только шапка «встала на место», последующий Deflate-декодер получает корректную точку старта — и не страдает от каскада ошибок ниже по потоку.

### 2. Stateful Beam Search с контентными оракулами (монолитные payload'ы)

Ключевое архитектурное решение Milestone 4. Пользуется тем, что `miniz_oxide::inflate::core::DecompressorOxide` (начиная с версии 0.8) реализует `Clone` — а значит, внутреннее состояние декомпрессора можно форкать.

Алгоритм:

- Поток сжатых данных подаётся декомпрессору **инкрементально**, чанками между LF-кандидатами.
- Состояние каждого «живого» кандидата (`BeamCandidate`): клон `DecompressorOxide`, клон `crc32fast::Hasher`, кольцевое 32 КБ окно LZ77, курсор во входном потоке, история принятых решений о вставке `\r`, опциональный контентный валидатор.
- На каждой позиции LF кандидат **форкается**: ветка A скармливает декомпрессору только `0x0A`, ветка B — `[0x0D, 0x0A]`.
- Кандидаты, у которых `miniz_oxide` возвращает `TINFLStatus::Failed` (битовый поток рассинхронизировался), немедленно отбрасываются.
- Пучок ограничен настраиваемой шириной (по умолчанию 2000, переопределяется переменной окружения `LOXAM_BEAM_WIDTH`). Каждый кандидат весит ≈145 КиБ, так что 5000 ≈ 725 МиБ, 10000 ≈ 1.5 ГиБ рабочей памяти.
- **Water-fill bucketed pruning.** Когда пучок превышает лимит, кандидаты бьются на «вёдра» по количеству вставленных `\r` (`inserts.len()`). Меньшие вёдра забираются целиком, освобождённый бюджет перераспределяется на большие. Внутри переполненного ведра применяется детерминированный partial-shuffle — это защищает правильную траекторию от вытеснения по tied `total_out` и стабильному порядку (внутри stored-блоков Deflate сотни кандидатов часто имеют одинаковый `total_out`).
- Декомпрессованные байты сразу **потоково** хэшируются в CRC32; полный uncompressed-буфер никогда не материализуется — на 308 МБ архиве потребление памяти остаётся в пределах пучок × 32 КБ ≈ 64 МБ.
- Победитель — кандидат, у которого декомпрессия завершилась корректно (`Done`), `total_out` совпал с ожидаемым размером, и CRC32 сошёлся.

### 3. Контентные оракулы (cut stored-block blind spot)

Stored-блоки Deflate (`BTYPE=00`) передают декодированные байты дословно, без Huffman-перекодировки. Это «слепое пятно» для бит-потокового оракула: любая ошибочная вставка `\r` внутри stored-блока проходит декомпрессор без сбоя, но повреждает выходные байты. Чтобы закрыть эту дыру, пучок проверяется тремя дополнительными оракулами:

- **File-signature prefix.** По расширению файла подбирается известная магическая сигнатура (PNG, JPEG, GIF, PDF, ZIP/JAR, GZ, BZ2, 7Z, MP3). Если первые N декодированных байт не совпадают с сигнатурой, кандидат убивается.
- **Trailer oracle.** Для типов с фиксированным «хвостом» (PNG IEND, JPEG EOI, GIF `;`) последние байты сравниваются с известным трейлером перед финальной CRC-проверкой.
- **Streaming PNG chunk validator.** Для `.png` файлов декодированный поток разбирается чанк за чанком (length → type → data → CRC32). Каждый чанк PNG несёт свой CRC32 — это внутренний оракул высокой точности. На `Scan_20260201.png` (4 МБ, 510 IDAT-чанков) это 510 независимых CRC-ворот, каждое из которых срезает все неправильные траектории в своём окне.

Оракулы **потоковые**: они работают по мере того, как miniz_oxide выплёвывает байты, и не требуют полной материализации декомпрессованного потока.

### 4. Feed-semantics alignment

`miniz_oxide::inflate::decompress_to_vec` принимает `TINFLStatus::Done` с неиспользованным входом на финальном вызове. Наша инкрементальная подача должна вести себя так же, иначе легитимные кандидаты убьются на последнем чанке. Реализовано: `Done` с остатком байт считается валидным только если `has_more_after=false`.

### 5. DFS-fallback для небольших секций

Для потоков с небольшим числом LF-кандидатов (сотни) beam search может потерять правильную ветку из-за вероятностной обрезки: `miniz_oxide`-декодер слишком лениво ловит ошибки в коротких стримах. Поэтому для таких случаев оставлен старый exhaustive-алгоритм `keep_one` / `keep_two` как резервный путь — он перебирает одиночные и парные вставки `\r` с полной перепроверкой CRC. Beam search вызывается первым; DFS подхватывает, если пучок опустел.

## Ключевые особенности

- **Zero-copy I/O через `memmap2`** — файлы любого размера открываются через `mmap`, без копирования в heap. Архив на 3 ГБ не потребует 3 ГБ памяти.
- **Параллельное восстановление через `rayon`** — каждый файл внутри архива обрабатывается независимо в своём потоке рабочего пула. На 8-ядерном CPU восстановление масштабируется почти линейно.
- **Stateful Beam Search на `miniz_oxide` 0.8** — клонируемое состояние декомпрессора, потоковое CRC32, 32 КБ LZ77-окно на кандидата, настраиваемая ширина через `LOXAM_BEAM_WIDTH`.
- **Водоналивная bucketed обрезка пучка** — квота на ведро по `inserts.len()`, детерминированный partial-shuffle внутри ведра. Защищает правильную траекторию от вытеснения по tied `total_out`.
- **Контентные оракулы** — file-signature prefix (PNG/JPEG/PDF/GIF/ZIP/GZ/BZ2/7Z/MP3), trailer oracle, streaming PNG chunk-CRC validator. Закрывают «слепое пятно» stored-блоков Deflate.
- **Поддержка ZIP64** — корректно разбирает и восстанавливает архивы с файлами > 4 ГБ и количеством записей > 65535 (extra field `0x0001`, 64-битные поля размера и смещения).
- **Прогресс-бар через `indicatif`** — для больших архивов выводится визуальный прогресс по этапам (сканирование заголовков, параллельное восстановление, сборка).
- **O(1)-memory оптимизации** — удалены промежуточные `O(N)` структуры (`CrlfLookup`, `offset_map`), заменены на инкрементальный merge-scan и prefix-sum mapping за `O(log M)`.

## Сборка и установка

Требуется Rust 2021 edition (stable, `rustc >= 1.70`).

```bash
git clone https://github.com/xelth-com/loxam.git
cd loxam
cargo build --release
```

Бинарник будет доступен по пути `target/release/loxam` (или `loxam.exe` на Windows).

Опционально — установить в `$PATH`:

```bash
cargo install --path .
```

## Использование

Утилита предоставляет четыре подкоманды.

### `recover` — восстановление повреждённого архива

Основная команда. Принимает на вход повреждённый файл, пишет восстановленный архив:

```bash
loxam recover broken.zip fixed.zip
```

После успешного завершения выводит стратегию, число попыток и per-file CRC-отчёт по всем записям архива.

### `corrupt` — эмуляция повреждения (для тестов)

Применяет преобразование `\n → \r\n` к произвольному файлу — удобно, чтобы вручную воспроизвести проблему FTP-передачи:

```bash
loxam corrupt original.zip broken.zip
```

### `test` — быстрый самотест

Создаёт небольшой ZIP из трёх сгенерированных текстовых файлов, ломает его и проверяет, что восстановление даёт побайтно-идентичный результат:

```bash
loxam test
loxam test --sizes 100 --sizes 200 --sizes 300
```

### `stress` — стресс-тест

Прогоняет N циклов «создать → сломать → восстановить» с заданным размером файлов. Полезно для проверки устойчивости алгоритма к случайным входным данным:

```bash
loxam stress --runs 100 --size 500
loxam stress --runs 20 --size 50000
```

Текущие бейзлайны: `100/100` на 500 Б, `20/20 Perfect` на 50 КБ.

## Переменные окружения

- **`LOXAM_BEAM_WIDTH`** (default: 2000) — ширина пучка для stateful beam search. Увеличивайте для тяжёлых payload'ов с большим количеством stored-блоков и натуральных CRLF. Каждый кандидат весит ≈145 КиБ, так что 5000 ≈ 725 МиБ, 10000 ≈ 1.5 ГиБ рабочей памяти.

```bash
LOXAM_BEAM_WIDTH=5000 loxam recover broken.zip fixed.zip
```

## Реальные результаты

- **`Scan_20260201.png` (4 062 804 байта сжатого потока, 510 IDAT-чанков).** Повреждено FTP: +16 640 LF-позиций, необходимо правильно расставить 44 естественных `\r`. С `LOXAM_BEAM_WIDTH=5000` восстановлено за 5h 29m (93 605 252 beam-попыток), результат **побайтно идентичен оригиналу** (CRC32 `AE8A16AD` сошёлся). На финальном feed пучок из 5000 кандидатов схлопнулся до единственного выжившего (4999 убиты PNG chunk CRC-валидатором).

## Ограничения

Честный список того, что `loxam` **не умеет** или умеет **не идеально**:

- **Поддерживается только метод сжатия Deflate (8) и Store (0).** BZIP2, LZMA, XZ, Zstd внутри ZIP — не обрабатываются.
- **Требуется целостность сигнатур `PK\x03\x04`.** Если сам 4-байтный магический маркер тоже попал под искажение (что маловероятно, так как он не содержит `0x0A`), стабилизация заголовка не сработает.
- **Зашифрованные ZIP** не восстанавливаются — оракул CRC32 проверяет расшифрованный поток, а ключ утилите неизвестен.
- **Нельзя восстановить двойную корраптацию.** Если файл пережил `\n→\r\n` дважды (`0x0A → 0x0D 0x0A → 0x0D 0x0D 0x0A`), потребуется два последовательных прогона — утилита не детектирует это автоматически.
- **Streaming content validator реализован только для PNG.** Для других типов работают только prefix/trailer оракулы + CRC32. Для JPEG / PDF / MP3 чанк-уровневая валидация сделает восстановление существенно надёжнее и быстрее.
- **Ширина beam search — эвристика.** На патологических payload'ах с плотно распределёнными натуральными CRLF и слабым Deflate-сигналом пучок может потерять правильную ветку. Рост `LOXAM_BEAM_WIDTH` снижает риск, но растёт память и время. Восстановление 4 МБ PNG потребовало ширины 5000.

## Лицензия

MIT.

---

<a name="english"></a>

# loxam (English)

**`loxam`** is a tool for recovering ZIP archives corrupted by FTP text-mode (ASCII mode) transfer. In that mode, an FTP client applies end-of-line translation to binary payload, turning every `0x0A` (`\n`) byte into the pair `0x0D 0x0A` (`\r\n`). For a hundreds-of-megabytes archive this means tens of thousands, or millions, of spurious bytes inserted — and the archive structure is completely destroyed.

Standard utilities (`unzip`, `7z`, `zip -FF`) give up on such files: they see shattered CRCs, broken headers and shifted offsets, and bail out.

`loxam` can undo the transformation and restore the original archive byte-for-byte.

## Description

The tool solves one specific but painful problem: **remove exactly the `\r` bytes that FTP inserted before each `\n`, without touching the "natural" `\r\n` sequences that incidentally occur inside compressed data.**

It handles ZIP archives of any size — from a few KB to multi-gigabyte — using zero-copy mmap I/O, parallel per-file recovery via `rayon`, and ZIP64 support. For monolithic payloads (a single huge compressed file with thousands of natural CRLFs), it applies a **Stateful Beam Search** on top of `miniz_oxide`'s cloneable decompressor state.

## The Problem

FTP text mode is designed for ASCII transfer between systems with different line-ending conventions (Unix: `\n`, Windows: `\r\n`, classic Mac: `\r`). When transferring from a Unix server to a Windows client, FTP applies:

```
every 0x0A byte  →  the pair 0x0D 0x0A
```

Harmless for text. Catastrophic for a binary ZIP:

1. **CRC32s are destroyed.** Any file whose compressed payload contained a `0x0A` byte gets extra `0x0D`s and fails integrity checks.
2. **Offsets shift.** The Central Directory stores absolute offsets of local headers. A single inserted byte shifts everything after it; the directory points into garbage.
3. **Deflate streams break.** Compression uses an unaligned bit stream — injecting one extra byte breaks Huffman decoding at the very first block.
4. **False positives.** Some `\r\n` sequences in the archive are **natural** — random coincidences inside compressed data, roughly 1 per 64 KB. Those must **not** be removed. Naively "strip every `\r` before `\n`" breaks about as many bytes as it fixes.

Typical numbers for a 380 MB archive: ~1,500,000 injected `\r` bytes to remove, ~6,000 natural `\r\n` sequences to keep. Full enumeration (`2^6000` combinations) is out of the question.

## How it works

`loxam` layers three complementary algorithms.

### 1. Header-First Stabilization

Around each local file header signature (`PK\x03\x04`) lie 16 critical bytes: version, flags, compression method, CRC32, sizes, name and extra-field lengths. If even one of them was touched by `\n→\r\n`, the downstream Deflate stream can't even begin to decode.

The fix:

- All `PK\x03\x04` signatures are located via `memchr` in a single mmap pass.
- For each signature, we enumerate all combinations of `\r` presence in the 16-byte header region (a small power of two — a few hundred candidates).
- Each candidate header is validated against known invariants: version field ∈ {10, 20, 45}, method ∈ {0, 8}, `compressed_size` consistent with the distance to the next signature, and so on.
- Once a header "snaps into place", the downstream Deflate decoder gets a clean starting point and is not derailed by cascading errors.

### 2. Stateful Beam Search with content oracles (monolithic payloads)

The key architectural decision of Milestone 4. It exploits the fact that `miniz_oxide::inflate::core::DecompressorOxide` (since 0.8) implements `Clone` — meaning decompressor state can be forked.

Algorithm:

- The compressed stream is fed to the decompressor **incrementally**, in chunks between LF candidates.
- Each live `BeamCandidate` holds: a clone of `DecompressorOxide`, a clone of `crc32fast::Hasher`, a 32 KiB rolling LZ77 window, its input cursor, a history of insertion decisions, and an optional streaming content validator.
- At every LF position the candidate **forks**: branch A feeds the decompressor just `0x0A`, branch B feeds `[0x0D, 0x0A]`.
- Any candidate for which `miniz_oxide` returns `TINFLStatus::Failed` (bit stream desynchronized) is dropped immediately.
- The beam has a configurable width (default 2000, overridable via the `LOXAM_BEAM_WIDTH` env var). Each candidate costs ≈145 KiB, so 5000 ≈ 725 MiB, 10000 ≈ 1.5 GiB of working memory.
- **Water-fill bucketed pruning.** When the beam exceeds capacity, candidates are bucketed by insertion count (`inserts.len()`). Smaller buckets are taken whole; freed budget is redistributed to larger ones. Within an over-capacity bucket a deterministic partial-shuffle protects the correct trajectory from stable-sort eviction on tied `total_out` — inside stored Deflate blocks hundreds of candidates can share identical decode progress.
- Decompressed bytes are **streamed** into the CRC32 hasher; the full uncompressed buffer is never materialized — peak RAM stays at beam × 32 KiB ≈ 64 MB on a 308 MB archive.
- The winner is the candidate whose decompression finished cleanly (`Done`), whose `total_out` matches the expected size, and whose CRC32 agrees with the header.

### 3. Content oracles (cut the stored-block blind spot)

Stored Deflate blocks (`BTYPE=00`) emit decoded bytes verbatim — no Huffman recoding. This is the bit-stream oracle's blind spot: a wrong `\r` insertion inside a stored block passes the decompressor but corrupts output bytes. Three extra oracles close the gap:

- **File-signature prefix.** By file extension we look up the known magic prefix (PNG, JPEG, GIF, PDF, ZIP/JAR, GZ, BZ2, 7Z, MP3). Candidates whose first N decoded bytes disagree are killed immediately.
- **Trailer oracle.** File types with a fixed tail (PNG IEND, JPEG EOI, GIF `;`) have their final bytes compared against the known trailer before the final CRC check.
- **Streaming PNG chunk validator.** For `.png` files the decoded stream is parsed chunk-by-chunk (length → type → data → CRC32). Every PNG chunk carries its own CRC32 — a high-precision internal oracle. On `Scan_20260201.png` (4 MB, 510 IDAT chunks) this gives 510 independent CRC gates, each of which cuts every wrong trajectory inside its window.

The oracles are **streaming**: they fire as miniz_oxide emits bytes and never require materializing the full decompressed stream.

### 4. Feed-semantics alignment

`miniz_oxide::inflate::decompress_to_vec` accepts `TINFLStatus::Done` with unused input on the final call. Our incremental feed has to match that semantics or legitimate candidates get killed on the last chunk. Implemented: `Done` with leftover bytes is accepted only when `has_more_after=false`.

### 5. DFS fallback for small sections

For streams with only a few hundred LF candidates, beam search can lose the right branch due to probabilistic pruning: `miniz_oxide`'s decoder catches errors too lazily over short streams. The old exhaustive `keep_one` / `keep_two` algorithm is kept as a safety net — it enumerates single and pair `\r` insertions with a full CRC check each time. Beam search runs first; DFS picks up when the beam empties.

## Key features

- **Zero-copy I/O via `memmap2`** — files of any size are opened via `mmap`, no heap copy. A 3 GB archive does not require 3 GB of RAM.
- **Parallel per-file recovery via `rayon`** — each file inside the archive is processed on its own worker thread. On an 8-core CPU recovery scales nearly linearly.
- **Stateful Beam Search on `miniz_oxide` 0.8** — cloneable decompressor state, streaming CRC32, 32 KiB LZ77 window per candidate, configurable width via `LOXAM_BEAM_WIDTH`.
- **Water-fill bucketed beam pruning** — per-bucket quota by `inserts.len()`, deterministic partial-shuffle within a bucket. Protects the correct trajectory from stable-sort eviction on tied `total_out`.
- **Content oracles** — file-signature prefix (PNG/JPEG/PDF/GIF/ZIP/GZ/BZ2/7Z/MP3), trailer oracle, streaming PNG chunk-CRC validator. Closes the stored-block blind spot in Deflate.
- **ZIP64 support** — correctly parses and recovers archives with files > 4 GB and > 65535 entries (extra field `0x0001`, 64-bit size and offset fields).
- **Progress bar via `indicatif`** — large archives get a visual progress indicator across header scan, parallel recovery, and assembly phases.
- **O(1)-memory optimizations** — intermediate `O(N)` structures (`CrlfLookup`, `offset_map`) replaced with an incremental merge-scan and `O(log M)` prefix-sum mapping.

## Build & install

Rust 2021 edition required (stable, `rustc >= 1.70`).

```bash
git clone https://github.com/xelth-com/loxam.git
cd loxam
cargo build --release
```

The binary lands at `target/release/loxam` (or `loxam.exe` on Windows).

Optional system-wide install:

```bash
cargo install --path .
```

## Usage

Four subcommands.

### `recover` — recover a corrupted archive

The primary command. Takes a corrupted input, writes a restored archive:

```bash
loxam recover broken.zip fixed.zip
```

On success it prints the strategy used, the number of attempts, and a per-file CRC report.

### `corrupt` — emulate the damage (for testing)

Applies the `\n → \r\n` transformation to an arbitrary file — useful for reproducing the FTP issue on demand:

```bash
loxam corrupt original.zip broken.zip
```

### `test` — quick self-test

Builds a small ZIP from three generated text files, corrupts it, and verifies that recovery yields a byte-identical result:

```bash
loxam test
loxam test --sizes 100 --sizes 200 --sizes 300
```

### `stress` — stress test

Runs N cycles of "generate → corrupt → recover" at a given file size. Useful for checking robustness on random inputs:

```bash
loxam stress --runs 100 --size 500
loxam stress --runs 20 --size 50000
```

Current baselines: `100/100` at 500 B, `20/20 Perfect` at 50 KB.

## Environment variables

- **`LOXAM_BEAM_WIDTH`** (default: 2000) — beam width for the stateful beam search. Bump it up for hard payloads with many stored blocks and natural CRLFs. Each candidate costs ≈145 KiB, so 5000 ≈ 725 MiB, 10000 ≈ 1.5 GiB of working memory.

```bash
LOXAM_BEAM_WIDTH=5000 loxam recover broken.zip fixed.zip
```

## Real-world results

- **`Scan_20260201.png` (4,062,804-byte compressed stream, 510 IDAT chunks).** FTP-corrupted with +16,640 LF positions, needing the correct placement of 44 natural `\r` bytes. With `LOXAM_BEAM_WIDTH=5000` recovery took 5h 29m (93,605,252 beam attempts), result **byte-identical to the original** (CRC32 `AE8A16AD` matched). On the final feed the 5000-wide beam collapsed to a single survivor (4999 killed by the PNG chunk-CRC validator).

## Limitations

An honest list of what `loxam` **cannot** or does **not** do perfectly:

- **Only Deflate (method 8) and Store (method 0) are supported.** BZIP2, LZMA, XZ, Zstd-inside-ZIP are not handled.
- **Signature integrity required.** If the 4-byte `PK\x03\x04` magic itself was corrupted (unlikely, since it contains no `0x0A`), header stabilization won't kick in.
- **Encrypted ZIPs** are out of scope — the CRC32 oracle validates the decrypted stream, and the tool doesn't know the key.
- **Double corruption is not auto-detected.** If a file survived `\n→\r\n` twice (`0x0A → 0x0D 0x0A → 0x0D 0x0D 0x0A`), two sequential passes are needed — the tool does not detect it automatically.
- **Streaming content validator is PNG-only.** Other file types fall back to prefix/trailer + CRC32 oracles. A chunk-level validator for JPEG / PDF / MP3 would make recovery substantially more reliable and faster.
- **Beam width is a heuristic.** On pathological payloads with densely packed natural CRLFs and weak Deflate signal, the beam may lose the correct branch. Increasing `LOXAM_BEAM_WIDTH` reduces the risk at the cost of memory and time. Recovering the 4 MB PNG above required width 5000.

## License

MIT.
