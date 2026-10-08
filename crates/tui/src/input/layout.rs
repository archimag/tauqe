use crate::config::LayoutPreset;

pub fn translate_char(c: char, preset: LayoutPreset, custom_langmap: Option<&str>) -> char {
    if let Some(lm) = custom_langmap {
        if let Some(mapped) = lookup_langmap(c, lm) {
            return mapped;
        }
    }
    match preset {
        LayoutPreset::None => c,
        LayoutPreset::RuJcuken => cyrillic_to_qwerty(c),
    }
}

/// Lookup character mapping in a custom langmap string.
/// Supports two standard formats:
/// 1. Semicolon format: "from_chars;to_chars" (e.g. "йцукен;qwerty")
/// 2. Pair format: "йq,цw" or "й:q,ц:w" or "й=q ц=w"
pub fn lookup_langmap(c: char, spec: &str) -> Option<char> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }

    // Format 1: "from_chars;to_chars"
    if let Some((from_part, to_part)) = spec.split_once(';') {
        let from_chars: Vec<char> = from_part.chars().collect();
        let to_chars: Vec<char> = to_part.chars().collect();
        if let Some(idx) = from_chars.iter().position(|&x| x == c) {
            if let Some(&target) = to_chars.get(idx) {
                return Some(target);
            }
        }
        let c_lower = c.to_lowercase().next()?;
        if let Some(idx) = from_chars.iter().position(|&x| x == c_lower) {
            if let Some(&target) = to_chars.get(idx) {
                return if c.is_uppercase() {
                    target.to_uppercase().next()
                } else {
                    Some(target)
                };
            }
        }
        return None;
    }

    // Format 2: Comma or space separated pairs: "йq,цw" / "й:q" / "й=q"
    for item in spec.split([',', ' ']) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (from_ch, to_ch) = if let Some((f, t)) = item.split_once(':') {
            (f.chars().next(), t.chars().next())
        } else if let Some((f, t)) = item.split_once('=') {
            (f.chars().next(), t.chars().next())
        } else {
            let mut chars = item.chars();
            let f = chars.next();
            let t = chars.next();
            (f, t)
        };

        if let (Some(f), Some(t)) = (from_ch, to_ch) {
            if f == c {
                return Some(t);
            }
            if f.to_lowercase().next() == c.to_lowercase().next() {
                return if c.is_uppercase() {
                    t.to_uppercase().next()
                } else {
                    Some(t)
                };
            }
        }
    }

    None
}

pub fn cyrillic_to_qwerty(c: char) -> char {
    match c {
        'й' => 'q',
        'Й' => 'Q',
        'ц' => 'w',
        'Ц' => 'W',
        'у' => 'e',
        'У' => 'E',
        'к' => 'r',
        'К' => 'R',
        'е' => 't',
        'Е' => 'T',
        'н' => 'y',
        'Н' => 'Y',
        'г' => 'u',
        'Г' => 'U',
        'ш' => 'i',
        'Ш' => 'I',
        'щ' => 'o',
        'Щ' => 'O',
        'з' => 'p',
        'З' => 'P',
        'х' => '[',
        'Х' => '{',
        'ъ' => ']',
        'Ъ' => '}',
        'ф' => 'a',
        'Ф' => 'A',
        'ы' => 's',
        'Ы' => 'S',
        'в' => 'd',
        'В' => 'D',
        'а' => 'f',
        'А' => 'F',
        'п' => 'g',
        'П' => 'G',
        'р' => 'h',
        'Р' => 'H',
        'о' => 'j',
        'О' => 'J',
        'л' => 'k',
        'Л' => 'K',
        'д' => 'l',
        'Д' => 'L',
        'ж' => ';',
        'Ж' => ':',
        'э' => '\'',
        'Э' => '"',
        'я' => 'z',
        'Я' => 'Z',
        'ч' => 'x',
        'Ч' => 'X',
        'с' => 'c',
        'С' => 'C',
        'м' => 'v',
        'М' => 'V',
        'и' => 'b',
        'И' => 'B',
        'т' => 'n',
        'Т' => 'N',
        'ь' => 'm',
        'Ь' => 'M',
        'б' => ',',
        'Б' => '<',
        'ю' => '.',
        'Ю' => '>',
        'ё' => '`',
        'Ё' => '~',
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lookup_langmap_semicolon() {
        let spec = "йцукен;qwerty";
        assert_eq!(lookup_langmap('й', spec), Some('q'));
        assert_eq!(lookup_langmap('Й', spec), Some('Q'));
        assert_eq!(lookup_langmap('у', spec), Some('e'));
        assert_eq!(lookup_langmap('z', spec), None);
    }

    #[test]
    fn test_lookup_langmap_pairs() {
        let spec = "й:q, ц:w, у:e";
        assert_eq!(lookup_langmap('й', spec), Some('q'));
        assert_eq!(lookup_langmap('Ц', spec), Some('W'));
        assert_eq!(lookup_langmap('у', spec), Some('e'));
        assert_eq!(lookup_langmap('х', spec), None);
    }

    #[test]
    fn test_translate_char_precedence() {
        // Custom langmap takes precedence over preset
        let custom = "й:z";
        assert_eq!(translate_char('й', LayoutPreset::RuJcuken, Some(custom)), 'z');
        // Falls back to preset if not in custom langmap
        assert_eq!(translate_char('ц', LayoutPreset::RuJcuken, Some(custom)), 'w');
    }
}
