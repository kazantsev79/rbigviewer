//! Документ (одна вкладка): навигация по строкам поверх блочного кэша.
//!
//! Позиция просмотра — смещение начала верхней строки в байтах (`top`).
//! Любая операция, которой не хватило данных, остаётся в `pending` и
//! повторяется в следующем кадре, когда фоновый поток дочитает блок.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use encoding_rs::Encoding;

use crate::encoding::{self, Nl};
use crate::index::Index;
use crate::source::{BLOCK, Missing, Repaint, Source};

/// Строка длиннее этого режется на несколько экранных строк.
/// Так любая операция читает не больше ~16 КБ, даже если в логе JSON на 200 МБ.
pub const MAX_LINE: u64 = 16 * 1024;
const TAB: &str = "    ";

pub struct Line {
    pub start: u64,
    /// Начало следующей экранной строки.
    pub next: u64,
    /// Строка порезана по MAX_LINE (продолжение на следующей экранной строке).
    pub split: bool,
    pub text: String,
}

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Scroll(i64),
    /// Перейти к произвольному байту (выравнивается на начало строки).
    Jump(u64),
    GotoLine(u64),
    End,
}

pub struct Document {
    pub path: PathBuf,
    pub name: String,
    src: Source,
    index: Index,
    repaint: Repaint,
    pub encoding: &'static Encoding,
    pub detected: &'static Encoding,
    bom: Option<(&'static Encoding, u64)>,
    data_start: u64,
    nl: Nl,
    pub top: u64,
    pending: Option<Action>,
    pub hscroll: usize,
    pub scroll_acc: f32,
    pub follow: bool,
    pub message: Option<String>,
    last_stat: Instant,
}

impl Document {
    pub fn open(path: &Path, repaint: Repaint) -> std::io::Result<Self> {
        let src = Source::open(path, repaint.clone())?;
        let det = encoding::detect(src.file(), src.size());
        let bom = (det.bom_len > 0).then_some((det.encoding, det.bom_len));
        let nl = Nl::of(det.encoding);
        let index = Index::start(src.file().try_clone()?, nl, repaint.clone());
        Ok(Self {
            name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            path: path.to_path_buf(),
            src,
            index,
            repaint,
            encoding: det.encoding,
            detected: det.encoding,
            bom,
            data_start: det.bom_len,
            nl,
            top: det.bom_len,
            pending: None,
            hscroll: 0,
            scroll_acc: 0.0,
            follow: false,
            message: None,
            last_stat: Instant::now(),
        })
    }

    pub fn size(&self) -> u64 {
        self.src.size()
    }

    pub fn set_encoding(&mut self, enc: &'static Encoding) {
        if enc == self.encoding {
            return;
        }
        self.encoding = enc;
        self.data_start = match self.bom {
            Some((bom_enc, len)) if bom_enc == enc => len,
            _ => 0,
        };
        let nl = Nl::of(enc);
        if nl != self.nl {
            // Перевод строки выглядит по-другому — индекс строк надо строить заново.
            self.nl = nl;
            if let Ok(f) = self.src.file().try_clone() {
                self.index = Index::start(f, nl, self.repaint.clone());
            }
        }
        self.push(Action::Jump(self.top));
    }

    pub fn push(&mut self, a: Action) {
        self.pending = match (self.pending, a) {
            (Some(Action::Scroll(x)), Action::Scroll(y)) => Some(Action::Scroll(x + y)),
            _ => Some(a),
        };
    }

    /// Вызывается каждый кадр до отрисовки.
    pub fn tick(&mut self, rows: usize) {
        if self.last_stat.elapsed() > Duration::from_millis(1000) {
            self.last_stat = Instant::now();
            let (old, new) = self.src.refresh_size();
            if new < old {
                self.message = Some("Файл стал меньше (ротация?) — перечитан".into());
                self.top = self.data_start;
                self.pending = None;
            }
            if new > old && self.follow {
                self.push(Action::End);
            }
        }
        if let Some(a) = self.pending {
            match self.run(a, rows) {
                Ok(()) => self.pending = None,
                Err(Missing) => {} // повторим, когда придут данные
            }
        }
    }

    pub fn is_busy(&self) -> bool {
        self.pending.is_some()
    }

    fn run(&mut self, a: Action, rows: usize) -> Result<(), Missing> {
        match a {
            Action::Scroll(n) => {
                let mut t = self.top;
                if n > 0 {
                    for _ in 0..n {
                        let l = self.line_at(t)?;
                        if l.next >= self.size() {
                            break;
                        }
                        t = l.next;
                    }
                } else {
                    for _ in 0..-n {
                        match self.prev_start(t)? {
                            Some(p) => t = p,
                            None => break,
                        }
                    }
                }
                self.top = t;
            }
            Action::Jump(pos) => self.top = self.line_start_at(pos)?,
            Action::End => {
                let mut t = self.prev_start(self.size())?.unwrap_or(self.data_start);
                for _ in 1..rows.max(1) {
                    match self.prev_start(t)? {
                        Some(p) => t = p,
                        None => break,
                    }
                }
                self.top = t;
            }
            Action::GotoLine(n) => match self.offset_of_line(n)? {
                Some(off) => self.top = off,
                None => self.message = Some(format!("Строка {n} ещё не проиндексирована")),
            },
        }
        Ok(())
    }

    fn align(&self, off: u64) -> u64 {
        let off = off.clamp(self.data_start, self.size().max(self.data_start));
        off - (off - self.data_start) % self.nl.unit()
    }

    /// Экранная строка, начинающаяся с `start`.
    pub fn line_at(&self, start: u64) -> Result<Line, Missing> {
        let unit = self.nl.unit();
        let span = self.src.read_span(start, MAX_LINE + unit)?;
        let (len, next, split) = match self.nl.find_first(&span) {
            Some(p) => (p, start + (p as u64) + unit, false),
            None if span.len() as u64 <= MAX_LINE => (span.len(), start + span.len() as u64, false),
            None => {
                let mut cut = MAX_LINE as usize;
                if self.nl == Nl::Byte && self.encoding == encoding_rs::UTF_8 {
                    // Не режем UTF-8 символ посередине.
                    while cut > 1 && span[cut] & 0xC0 == 0x80 && MAX_LINE as usize - cut < 3 {
                        cut -= 1;
                    }
                }
                (cut, start + cut as u64, true)
            }
        };
        Ok(Line { start, next, split, text: self.decode(&span[..len]) })
    }

    fn decode(&self, bytes: &[u8]) -> String {
        let (text, _) = self.encoding.decode_without_bom_handling(bytes);
        let text = text.strip_suffix('\r').unwrap_or(&text);
        let mut out = String::with_capacity(text.len());
        for c in text.chars() {
            match c {
                '\t' => out.push_str(TAB),
                c if c.is_control() => out.push('·'),
                c => out.push(c),
            }
        }
        out
    }

    /// Начало предыдущей экранной строки (перед `off`, который сам начало строки).
    fn prev_start(&self, off: u64) -> Result<Option<u64>, Missing> {
        if off <= self.data_start {
            return Ok(None);
        }
        let unit = self.nl.unit();
        let lo = self.align(off.saturating_sub(MAX_LINE + unit));
        // Последний символ перед off — перевод строки, закрывающий предыдущую строку.
        let span = self.src.read_span(lo, off.saturating_sub(unit).saturating_sub(lo))?;
        Ok(Some(match self.nl.find_last(&span) {
            Some(p) => lo + p as u64 + unit,
            None if lo == self.data_start => self.data_start,
            None => self.align(off - MAX_LINE),
        }))
    }

    /// Начало строки, внутри которой находится байт `pos`.
    fn line_start_at(&self, pos: u64) -> Result<u64, Missing> {
        let pos = self.align(pos);
        if pos >= self.size() {
            return Ok(self.prev_start(self.size())?.unwrap_or(self.data_start));
        }
        let lo = self.align(pos.saturating_sub(MAX_LINE));
        let span = self.src.read_span(lo, pos - lo)?;
        Ok(match self.nl.find_last(&span) {
            Some(p) => lo + p as u64 + self.nl.unit(),
            None if lo == self.data_start => self.data_start,
            None => pos,
        })
    }

    /// Строки для экрана начиная с `top`. Второй элемент — всё ли загружено.
    pub fn visible_lines(&self, rows: usize) -> (Vec<Line>, bool) {
        let mut out = Vec::with_capacity(rows);
        let mut t = self.top;
        let size = self.size();
        while out.len() < rows && t < size {
            match self.line_at(t) {
                Ok(l) => {
                    t = l.next;
                    out.push(l);
                }
                Err(Missing) => return (out, false),
            }
        }
        // Предзагрузка соседних блоков, чтобы прокрутка не ждала диск.
        let first = self.top / BLOCK;
        if first > 0 {
            self.src.request(first - 1);
        }
        self.src.request(t / BLOCK + 1);
        (out, true)
    }

    /// Номер строки (с единицы) для смещения начала строки, если индекс уже дошёл.
    pub fn line_number(&self, off: u64) -> Option<u64> {
        let b = off / BLOCK;
        let base = {
            let d = self.index.data.lock().unwrap();
            if (b as usize) >= d.checkpoints.len() || off > d.indexed {
                return None;
            }
            d.checkpoints[b as usize]
        };
        let span = self.src.read_span(b * BLOCK, off - b * BLOCK).ok()?;
        Some(base + self.nl.count(&span) + 1)
    }

    fn offset_of_line(&self, n: u64) -> Result<Option<u64>, Missing> {
        let target = n.saturating_sub(1); // столько переводов строк до нужной строки
        if target == 0 {
            return Ok(Some(self.data_start));
        }
        let d = self.index.snapshot();
        let total = d.total_newlines();
        let target = if target > total {
            if !d.complete {
                return Ok(None);
            }
            // За концом файла — встаём на последнюю строку.
            total.saturating_sub(d.ends_with_nl as u64)
        } else {
            target
        };
        if target == 0 {
            return Ok(Some(self.data_start));
        }
        let b = d.checkpoints.partition_point(|&c| c < target) - 1;
        let k = target - d.checkpoints[b];
        let bstart = b as u64 * BLOCK;
        let span = self.src.read_span(bstart, BLOCK)?;
        Ok(Some(match self.nl.find_nth(&span, k) {
            Some(p) => bstart + p as u64 + self.nl.unit(),
            None => bstart,
        }))
    }

    /// (всего строк на данный момент, доля проиндексированного, индекс дошёл до конца)
    pub fn index_info(&self) -> (u64, f32, bool) {
        let d = self.index.data.lock().unwrap();
        let size = self.size();
        let mut lines = d.total_newlines();
        if d.complete && size > self.data_start && !d.ends_with_nl {
            lines += 1;
        }
        let progress = if size == 0 { 1.0 } else { d.indexed as f32 / size as f32 };
        (lines, progress.min(1.0), d.complete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;

    fn retry<T>(mut f: impl FnMut() -> Result<T, Missing>) -> T {
        for _ in 0..10_000 {
            if let Ok(v) = f() {
                return v;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("данные так и не загрузились");
    }

    /// Строки разной длины, включая очень длинные, с переходами через границы блоков.
    fn sample_lines() -> Vec<String> {
        let mut seed = 12345u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        (0..20_000)
            .map(|i| {
                let len = match rnd() % 100 {
                    0 => 40_000 + (rnd() % 30_000) as usize, // длинная строка
                    1 => 0,
                    _ => (rnd() % 300) as usize,
                };
                format!("{i:06} ") + &"ж".repeat(len / 2)
            })
            .collect()
    }

    fn write_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("rbigviewer-test-{}-{name}", std::process::id()));
        std::fs::File::create(&path).unwrap().write_all(bytes).unwrap();
        path
    }

    fn check(name: &str, bytes: Vec<u8>, starts: Vec<u64>) {
        let path = write_file(name, &bytes);
        let doc = Document::open(&path, Arc::new(|| {})).unwrap();

        // Вперёд: начала «настоящих» строк совпадают с эталоном.
        let mut t = doc.data_start;
        let mut real = vec![];
        let mut cont = false;
        while t < doc.size() {
            let l = retry(|| doc.line_at(t));
            if !cont {
                real.push(t);
            }
            cont = l.split;
            t = l.next;
        }
        assert_eq!(real, starts, "{name}: вперёд");

        // Назад от конца проходим через все начала строк.
        let mut t = doc.size();
        let mut back = vec![];
        while let Some(p) = retry(|| doc.prev_start(t)) {
            back.push(p);
            t = p;
        }
        back.reverse();
        let mut it = back.iter();
        for s in &starts {
            assert!(it.any(|b| b == s), "{name}: назад не прошли через {s}");
        }

        // Индекс: переход к строке и номер строки.
        while !doc.index_info().2 {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(doc.index_info().0, starts.len() as u64);
        for n in [1, 2, 17, 999, 5000, 12_345, starts.len() as u64] {
            let off = retry(|| doc.offset_of_line(n)).unwrap();
            assert_eq!(off, starts[n as usize - 1], "{name}: строка {n}");
            assert_eq!(retry(|| doc.line_number(off).ok_or(Missing)), n);
        }

        // Произвольный прыжок встаёт на начало строки.
        for pos in [0, 1, 777_777, doc.size() / 2, doc.size() - 1, doc.size()] {
            let s = retry(|| doc.line_start_at(pos));
            assert!(s <= pos.max(doc.data_start) || pos >= doc.size(), "{name}: прыжок {pos}");
        }
        drop(doc);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn navigation_utf8() {
        let lines = sample_lines();
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        let mut starts = vec![];
        for l in &lines {
            starts.push(bytes.len() as u64);
            bytes.extend_from_slice(l.as_bytes());
            bytes.extend_from_slice(b"\r\n");
        }
        check("utf8", bytes, starts);
    }

    #[test]
    fn navigation_utf16le() {
        let lines = sample_lines();
        let mut bytes = vec![0xFF, 0xFE];
        let mut starts = vec![];
        for (i, l) in lines.iter().enumerate() {
            starts.push(bytes.len() as u64);
            let nl = if i + 1 == lines.len() { "" } else { "\n" }; // без перевода строки в конце
            bytes.extend(format!("{l}{nl}").encode_utf16().flat_map(|u| u.to_le_bytes()));
        }
        check("utf16", bytes, starts);
    }
}
