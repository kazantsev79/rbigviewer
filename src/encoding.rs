//! Определение кодировки и поиск переводов строк с учётом кодировки.

use encoding_rs::Encoding;
use std::fs::File;

use crate::source::read_at;

/// Как выглядит перевод строки в байтах.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nl {
    /// `0A` — UTF-8 и все однобайтовые кодировки.
    Byte,
    /// `0A 00`
    U16Le,
    /// `00 0A`
    U16Be,
}

impl Nl {
    pub fn of(enc: &'static Encoding) -> Nl {
        if enc == encoding_rs::UTF_16LE {
            Nl::U16Le
        } else if enc == encoding_rs::UTF_16BE {
            Nl::U16Be
        } else {
            Nl::Byte
        }
    }

    /// Размер одной «единицы» текста в байтах (для выравнивания смещений).
    pub fn unit(self) -> u64 {
        match self {
            Nl::Byte => 1,
            _ => 2,
        }
    }

    /// Позиции начала всех переводов строк в `buf`.
    /// Для UTF-16 `buf` должен начинаться с чётного смещения в файле.
    fn iter(self, buf: &[u8]) -> impl DoubleEndedIterator<Item = usize> + '_ {
        memchr::memchr_iter(b'\n', buf).filter_map(move |p| match self {
            Nl::Byte => Some(p),
            Nl::U16Le => (p % 2 == 0 && buf.get(p + 1) == Some(&0)).then_some(p),
            Nl::U16Be => (p % 2 == 1 && buf[p - 1] == 0).then_some(p - 1),
        })
    }

    pub fn count(self, buf: &[u8]) -> u64 {
        match self {
            Nl::Byte => memchr::memchr_iter(b'\n', buf).count() as u64,
            _ => self.iter(buf).count() as u64,
        }
    }

    pub fn find_first(self, buf: &[u8]) -> Option<usize> {
        self.iter(buf).next()
    }

    pub fn find_last(self, buf: &[u8]) -> Option<usize> {
        match self {
            Nl::Byte => memchr::memrchr(b'\n', buf),
            _ => self.iter(buf).next_back(),
        }
    }

    /// Позиция n-го (с единицы) перевода строки.
    pub fn find_nth(self, buf: &[u8], n: u64) -> Option<usize> {
        self.iter(buf).nth(n.checked_sub(1)? as usize)
    }
}

/// Кодировки, которые предлагаются в списке для ручного выбора.
pub const CHOICES: &[&Encoding] = &[
    encoding_rs::UTF_8,
    encoding_rs::UTF_16LE,
    encoding_rs::UTF_16BE,
    encoding_rs::WINDOWS_1251,
    encoding_rs::KOI8_R,
    encoding_rs::KOI8_U,
    encoding_rs::IBM866,
    encoding_rs::ISO_8859_5,
    encoding_rs::WINDOWS_1252,
    encoding_rs::WINDOWS_1250,
    encoding_rs::WINDOWS_1257,
    encoding_rs::SHIFT_JIS,
    encoding_rs::GBK,
];

pub struct Detected {
    pub encoding: &'static Encoding,
    /// Длина BOM, если он есть — с этого места начинается текст.
    pub bom_len: u64,
}

const SAMPLE: u64 = 64 * 1024;

/// Определить кодировку по трём образцам: начало, середина, конец файла.
pub fn detect(file: &File, size: u64) -> Detected {
    let mut parts = Vec::new();
    let mut offsets = vec![0];
    if size > SAMPLE * 3 {
        offsets.push(size / 2 & !1);
        offsets.push((size - SAMPLE) & !1);
    }
    for off in offsets {
        let mut buf = vec![0u8; SAMPLE.min(size - off) as usize];
        let n = read_at(file, &mut buf, off).unwrap_or(0);
        buf.truncate(n);
        parts.push(buf);
    }
    detect_parts(&parts)
}

pub fn detect_parts(parts: &[Vec<u8>]) -> Detected {
    let first = parts.first().map(Vec::as_slice).unwrap_or(&[]);

    // 1. BOM
    if let Some((enc, bom_len)) = Encoding::for_bom(first) {
        return Detected { encoding: enc, bom_len: bom_len as u64 };
    }

    // 2. UTF-16 без BOM: много нулей на чётных или нечётных позициях.
    let probe = &first[..first.len().min(8192) & !1];
    if probe.len() >= 64 {
        let pairs = probe.len() / 2;
        let zeros_even = probe.iter().step_by(2).filter(|&&b| b == 0).count();
        let zeros_odd = probe.iter().skip(1).step_by(2).filter(|&&b| b == 0).count();
        if zeros_odd * 10 > pairs * 3 && zeros_even * 20 < pairs {
            return Detected { encoding: encoding_rs::UTF_16LE, bom_len: 0 };
        }
        if zeros_even * 10 > pairs * 3 && zeros_odd * 20 < pairs {
            return Detected { encoding: encoding_rs::UTF_16BE, bom_len: 0 };
        }
    }

    // 3. Корректный UTF-8 во всех образцах (чистый ASCII тоже считаем UTF-8).
    let utf8_ok = parts.iter().enumerate().all(|(i, p)| {
        // Образцы из середины могут начинаться с середины символа.
        let p = if i == 0 { &p[..] } else { skip_continuation(p) };
        match std::str::from_utf8(p) {
            Ok(_) => true,
            // Обрезанный в конце образца символ — не ошибка.
            Err(e) => e.error_len().is_none(),
        }
    });
    if utf8_ok {
        return Detected { encoding: encoding_rs::UTF_8, bom_len: 0 };
    }

    // 4. Однобайтовые кодировки — статистический детектор из Firefox.
    let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    for p in parts {
        det.feed(p, false);
    }
    det.feed(&[], true);
    let encoding = det.guess(None, chardetng::Utf8Detection::Deny);
    Detected { encoding, bom_len: 0 }
}

fn skip_continuation(p: &[u8]) -> &[u8] {
    let skip = p.iter().take(3).take_while(|&&b| b & 0xC0 == 0x80).count();
    &p[skip..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(bytes: &[u8]) -> &'static str {
        detect_parts(&[bytes.to_vec()]).encoding.name()
    }

    #[test]
    fn detects_common_encodings() {
        let text = "2026-11-24 14:30:00 ОШИБКА: соединение с базой данных потеряно, повтор через 5 секунд\n".repeat(20);
        assert_eq!(det(text.as_bytes()), "UTF-8");
        let (cp1251, _, _) = encoding_rs::WINDOWS_1251.encode(&text);
        assert_eq!(det(&cp1251), "windows-1251");
        let (koi, _, _) = encoding_rs::KOI8_R.encode(&text);
        // chardetng отдаёт KOI8-U — для русского текста она совпадает с KOI8-R.
        assert!(det(&koi).starts_with("KOI8"));
        let utf16le: Vec<u8> = text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(det(&utf16le), "UTF-16LE");
        let utf16be: Vec<u8> = text.encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        assert_eq!(det(&utf16be), "UTF-16BE");
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(text.as_bytes());
        assert_eq!(detect_parts(&[bom]).bom_len, 3);
    }

    #[test]
    fn utf16_newlines() {
        let le: Vec<u8> = "a\nਊ\nb".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        // 'ਊ' = U+0A0A — его байты не должны считаться переводом строки.
        assert_eq!(Nl::U16Le.count(&le), 2);
        let be: Vec<u8> = "a\nĊ\nb".encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        assert_eq!(Nl::U16Be.count(&be), 2);
        assert_eq!(Nl::U16Be.find_first(&be), Some(2));
    }
}
