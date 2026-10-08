//! Ключи сравнения строк плейлиста (§3.1, §6.14, ТЗ-43, ОВС-3 а).

use std::cmp::Ordering;

/// Текст после trim, to_lowercase и NFC (ОВС-3 а), разбитый на текстовые и числовые отрезки (§3.1).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TextKey(Box<[Seg]>);

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Seg {
    Text(Box<str>),
    /// ASCII-цифры без ведущих нулей; пустая строка — значение 0.
    Num(Box<str>),
}

impl TextKey {
    /// Строит ключ сравнения из исходной строки (§6.14): trim, to_lowercase, NFC,
    /// затем разбиение на максимальные пробеги ASCII-цифр (`Num`, без ведущих нулей)
    /// и остальных символов (`Text`).
    pub fn new(raw: &str) -> TextKey {
        let trimmed = raw.trim().to_lowercase();
        let normalized = icu_normalizer::ComposingNormalizerBorrowed::new_nfc().normalize(&trimmed);

        let mut segments = Vec::new();
        let mut chars = normalized.chars().peekable();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_digit() {
                let mut digits = String::new();
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_digit() {
                        digits.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let stripped = digits.trim_start_matches('0');
                segments.push(Seg::Num(stripped.into()));
            } else {
                let mut text = String::new();
                while let Some(&t) = chars.peek() {
                    if t.is_ascii_digit() {
                        break;
                    }
                    text.push(t);
                    chars.next();
                }
                segments.push(Seg::Text(text.into()));
            }
        }

        TextKey(segments.into())
    }

    /// `true`, если ключ не содержит отрезков (исходная строка была пустой после trim).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Натуральное сравнение ключей (§6.14): по отрезкам, `Num` сравниваются по длине и
/// затем лексикографически (= по числовому значению без переполнения), `Text` — по
/// кодовым точкам, `Num` всегда меньше `Text`; при равенстве общих отрезков короче ключ меньше.
pub fn cmp_text(a: &TextKey, b: &TextKey) -> Ordering {
    for (sa, sb) in a.0.iter().zip(b.0.iter()) {
        let ord = match (sa, sb) {
            (Seg::Num(na), Seg::Num(nb)) => na.len().cmp(&nb.len()).then_with(|| na.cmp(nb)),
            (Seg::Text(ta), Seg::Text(tb)) => ta.cmp(tb),
            (Seg::Num(_), Seg::Text(_)) => Ordering::Less,
            (Seg::Text(_), Seg::Num(_)) => Ordering::Greater,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.0.len().cmp(&b.0.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_key_splits_numbers_and_strips_zeros() {
        let key = TextKey::new("Track 007 - Intro");
        assert_eq!(
            key.0.as_ref(),
            &[
                Seg::Text("track ".into()),
                Seg::Num("7".into()),
                Seg::Text(" - intro".into()),
            ]
        );
    }

    #[test]
    fn text_key_nfc_equal() {
        let precomposed = TextKey::new("caf\u{00e9}");
        let decomposed = TextKey::new("cafe\u{0301}");
        assert_eq!(precomposed, decomposed);
    }

    #[test]
    fn cmp_text_natural_order() {
        let a = TextKey::new("no. 9");
        let b = TextKey::new("no. 10");
        assert_eq!(cmp_text(&a, &b), Ordering::Less);
    }

    #[test]
    fn text_key_empty_after_trim() {
        let key = TextKey::new("   ");
        assert!(key.is_empty());
    }
}
