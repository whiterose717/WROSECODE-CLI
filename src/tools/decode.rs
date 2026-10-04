//! The `decode` tool (spec PHASE 5): one-pass encoding sweep with nested
//! chains. Base64/32/58/85, hex, rot/Caesar, atbash, XOR single-byte, URL,
//! gzip/zlib/bz2/xz (via installed system tools, degrading gracefully),
//! morse, and binary — re-run recursively up to a depth limit so common
//! `base64(hex(...))` nests resolve in a single call.

use crate::tools::shell;
use anyhow::{bail, Result};
use base64::Engine as _;
use std::collections::HashSet;
use std::path::Path;

const PREVIEW: usize = 240;
const MAX_RESULTS: usize = 80;

/// Sweep `text` (or the file at `path`) through every known encoding, then
/// recursively through the outputs up to `depth` levels.
pub async fn sweep(
    text: Option<&str>,
    path: Option<&str>,
    depth: u64,
    root: &Path,
) -> Result<String> {
    let depth = depth.clamp(1, 6) as usize;
    let (mut queue, first_bytes) = match (text, path) {
        (Some(text), _) => (vec![(vec!["input".to_string()], text.to_string())], None),
        (None, Some(path)) => {
            let resolved = crate::tools::fs::resolve(root, path)?;
            let bytes = std::fs::read(&resolved)?;
            let lossy = String::from_utf8_lossy(&bytes).into_owned();
            (vec![(vec!["input".to_string()], lossy)], Some(bytes))
        }
        _ => bail!("decode needs `text` or `path`"),
    };

    let mut results: Vec<(Vec<String>, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Some(first) = queue.first() {
        seen.insert(first.1.clone());
    }

    // File-level compression: magic bytes decide which system tool to try.
    if let Some(bytes) = first_bytes {
        if let Some((label, decoded)) = decompress(&bytes, root).await? {
            if seen.insert(decoded.clone()) {
                queue.push((vec![label], decoded));
            }
        }
    }

    let mut index = 0;
    while index < queue.len() {
        let (chain, value) = queue[index].clone();
        index += 1;
        if chain.len() > depth + 1 {
            continue;
        }
        for (label, decoded) in decoders(&value) {
            if decoded.is_empty() || seen.contains(&decoded) {
                continue;
            }
            seen.insert(decoded.clone());
            let mut next_chain = chain.clone();
            next_chain.push(label);
            results.push((next_chain.clone(), decoded.clone()));
            queue.push((next_chain, decoded));
            if results.len() >= MAX_RESULTS {
                break;
            }
        }
        if results.len() >= MAX_RESULTS {
            break;
        }
    }

    let candidates: HashSet<String> = queue
        .iter()
        .map(|(_, value)| value.as_str())
        .chain(results.iter().map(|(_, value)| value.as_str()))
        .filter_map(flag_candidate)
        .collect();

    let mut report = format!(
        "decode sweep · depth ≤ {depth} · {} result(s)\n",
        results.len()
    );
    if let Some((chain, value)) = queue.first() {
        report.push_str(&format!("[{}] {}\n", chain.join(" → "), preview(value)));
    }
    for (chain, value) in results.iter().take(40) {
        report.push_str(&format!("{} → {}\n", chain.join(" → "), preview(value)));
    }
    if candidates.is_empty() {
        report.push_str("flag candidates: none in this sweep\n");
    } else {
        for candidate in candidates.iter().take(10) {
            report.push_str(&format!("flag candidate: {candidate}\n"));
        }
    }
    Ok(report)
}

fn preview(value: &str) -> String {
    let single_line: String = value
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .collect();
    if single_line.chars().count() > PREVIEW {
        let clipped: String = single_line.chars().take(PREVIEW).collect();
        format!("`{clipped}…` ({} chars)", value.chars().count())
    } else {
        format!("`{single_line}`")
    }
}

/// Every textual decoder applied to one value; each returns its own label so
/// chains read like `input → base64 → hex`.
fn decoders(value: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let trimmed = value.trim();

    if looks_base64(trimmed) {
        let cleaned = strip_whitespace(trimmed);
        for (label, decoded) in [
            (
                "base64",
                base64::engine::general_purpose::STANDARD.decode(&cleaned),
            ),
            (
                "base64url",
                base64::engine::general_purpose::URL_SAFE.decode(&cleaned),
            ),
            (
                "base64-nopad",
                base64::engine::general_purpose::STANDARD_NO_PAD.decode(strip_padding(&cleaned)),
            ),
        ] {
            if let Ok(bytes) = decoded {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if !text.is_empty() && text.chars().all(|ch| !ch.is_control() || ch == '\n') {
                    out.push((label.to_string(), text));
                    break;
                }
            }
        }
    }
    if looks_base32(trimmed) {
        if let Some(text) = base32_decode(strip_whitespace(trimmed).as_str()) {
            out.push(("base32".to_string(), text));
        }
    }
    if let Some(decoded) = base58_decode(trimmed) {
        out.push(("base58".to_string(), decoded));
    }
    if let Some(decoded) = ascii85_decode(trimmed) {
        out.push(("base85/ascii85".to_string(), decoded));
    }
    if let Some(decoded) = hex_decode(trimmed) {
        out.push(("hex".to_string(), decoded));
    }
    if trimmed.contains('%') || trimmed.contains('+') {
        if let Some(decoded) = url_decode(trimmed) {
            out.push(("url".to_string(), decoded));
        }
    }
    if trimmed.chars().any(|ch| ch.is_ascii_alphabetic()) {
        out.push(("rot13".to_string(), rot_n(trimmed, 13)));
        if let Some(best) = best_caesar(trimmed) {
            out.push((format!("caesar(rot{best})"), rot_n(trimmed, best)));
        }
        out.push(("atbash".to_string(), atbash(trimmed)));
    }
    if trimmed.chars().any(|ch| ch.is_ascii_digit()) {
        if let Some(decoded) = binary_decode(trimmed) {
            out.push(("binary".to_string(), decoded));
        }
        if let Some((label, decoded)) = xor_single(trimmed) {
            out.push((label, decoded));
        }
    }
    let morse_shaped = trimmed
        .chars()
        .all(|ch| matches!(ch, '.' | '-' | '/' | ' ' | '\n' | '\t'))
        && trimmed.chars().any(|ch| ch == '.' || ch == '-');
    if morse_shaped {
        if let Some(decoded) = morse_decode(trimmed) {
            out.push(("morse".to_string(), decoded));
        }
    }
    out.retain(|(_, value)| !value.is_empty());
    out
}

fn strip_whitespace(value: &str) -> String {
    value.chars().filter(|ch| !ch.is_whitespace()).collect()
}

fn strip_padding(value: &str) -> String {
    value.trim_end_matches('=').to_string()
}

fn looks_base64(value: &str) -> bool {
    let cleaned = strip_whitespace(value);
    cleaned.len() >= 8
        && cleaned.chars().filter(|ch| *ch != '=').count() >= 8
        && cleaned
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '=' | '-' | '_'))
}

fn looks_base32(value: &str) -> bool {
    let cleaned = strip_whitespace(value);
    cleaned.len() >= 16
        && cleaned
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '=')
}

/// RFC 4648 base32, hand-rolled (the base64 crate has no 32-bit alphabet).
fn base32_decode(value: &str) -> Option<String> {
    let mut bits: u32 = 0;
    let mut bit_count = 0u32;
    let mut bytes = Vec::new();
    for ch in value.chars() {
        if ch == '=' {
            break;
        }
        let digit = if ch.is_ascii_uppercase() {
            ch as u32 - 'A' as u32
        } else if ('2'..='7').contains(&ch) {
            26 + (ch as u32 - '2' as u32)
        } else {
            return None;
        };
        bits = (bits << 5) | digit;
        bit_count += 5;
        if bit_count >= 8 {
            bit_count -= 8;
            bytes.push(((bits >> bit_count) & 0xff) as u8);
        }
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.chars().all(|ch| !ch.is_control() || ch == '\n') {
        Some(text)
    } else {
        None
    }
}

fn hex_decode(value: &str) -> Option<String> {
    let cleaned = value
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    if cleaned.len() < 4 || !cleaned.len().is_multiple_of(2) {
        return None;
    }
    if !cleaned.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let bytes: Vec<u8> = (0..cleaned.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    let text = String::from_utf8(bytes).ok()?;
    if text.chars().all(|ch| !ch.is_control() || ch == '\n') {
        Some(text)
    } else {
        None
    }
}

fn url_decode(value: &str) -> Option<String> {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    let mut changed = false;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                index += 3;
                changed = true;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
                changed = true;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    if !changed {
        return None;
    }
    let text = String::from_utf8(out).ok()?;
    if text.chars().all(|ch| !ch.is_control() || ch == '\n') {
        Some(text)
    } else {
        None
    }
}

fn rot_n(value: &str, shift: u8) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' => (((ch as u8 - b'a' + shift) % 26) + b'a') as char,
            'A'..='Z' => (((ch as u8 - b'A' + shift) % 26) + b'A') as char,
            other => other,
        })
        .collect()
}

fn atbash(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' => (b'z' - (ch as u8 - b'a')) as char,
            'A'..='Z' => (b'Z' - (ch as u8 - b'A')) as char,
            other => other,
        })
        .collect()
}

/// The Caesar shift whose plain text fits English letter frequencies best
/// (chi-squared) — CTFs rarely want all 25 shifts printed.
fn best_caesar(value: &str) -> Option<u8> {
    // A·(1/4) D·(1/4) E·(1/4) H·(1/4) I·(1/4) L·(1/4) N·(1/4) O·(1/4)
    // S·(1/4) T·(1/4) — scaled expected frequency of the ten commonest
    // English letters, in percent.
    const EXPECTED: [f64; 26] = [
        8.2, 1.5, 2.8, 4.3, 12.7, 2.2, 2.0, 6.1, 7.0, 0.15, 0.77, 4.0, 2.4, 6.7, 7.5, 1.9, 0.95,
        6.0, 6.3, 9.1, 2.8, 1.0, 2.4, 0.15, 2.0, 0.07,
    ];
    let letters: Vec<char> = value
        .chars()
        .filter(|ch| ch.is_ascii_alphabetic())
        .map(|ch| ch.to_ascii_lowercase())
        .collect();
    if letters.len() < 8 {
        return None;
    }
    let mut best: Option<(u8, f64)> = None;
    for shift in 1..26u8 {
        let mut observed = [0.0f64; 26];
        for &ch in &letters {
            let shifted = (ch as u8 - b'a' + shift) % 26;
            observed[shifted as usize] += 1.0;
        }
        let total = letters.len() as f64;
        let score: f64 = (0..26)
            .map(|index| {
                let expected = EXPECTED[index] / 100.0 * total;
                let diff = observed[index] - expected;
                diff * diff / expected.max(1e-9)
            })
            .sum();
        if best.is_none_or(|(_, best_score)| score < best_score) {
            best = Some((shift, score));
        }
    }
    best.map(|(shift, _)| shift)
}

fn base58_decode(value: &str) -> Option<String> {
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    if value.len() < 6 || !value.chars().all(|ch| ALPHABET.contains(&(ch as u8))) {
        return None;
    }
    let mut bytes: Vec<u8> = Vec::new();
    for byte in value.bytes() {
        let mut carry = ALPHABET.iter().position(|&alpha| alpha == byte)? as u32;
        for slot in bytes.iter_mut().rev() {
            let product = (*slot as u32) * 58 + carry;
            *slot = (product & 0xff) as u8;
            carry = product >> 8;
        }
        while carry > 0 {
            bytes.insert(0, (carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    for byte in value.bytes() {
        if byte != b'1' {
            break;
        }
        bytes.insert(0, 0);
    }
    let text = String::from_utf8(bytes).ok()?;
    if text
        .chars()
        .all(|ch| !ch.is_control() || ch == '\n' || ch == '\r' || ch == '\t')
        && !text.is_empty()
    {
        Some(text)
    } else {
        None
    }
}

fn ascii85_decode(value: &str) -> Option<String> {
    let body: String = value
        .trim_start_matches("<~")
        .trim_end_matches("~>")
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    if body.len() < 5 {
        return None;
    }
    let mut out = Vec::new();
    let chars: Vec<char> = body.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == 'z' {
            out.extend_from_slice(&[0, 0, 0, 0]);
            index += 1;
            continue;
        }
        if index + 5 > chars.len() {
            break;
        }
        let mut value: u32 = 0;
        for &ch in &chars[index..index + 5] {
            if !(33..=117).contains(&(ch as u32)) {
                return None;
            }
            value = value.checked_mul(85)?.checked_add(ch as u32 - 33)?;
        }
        out.extend_from_slice(&value.to_be_bytes());
        index += 5;
    }
    let text = String::from_utf8(out).ok()?;
    if text
        .chars()
        .all(|ch| !ch.is_control() || ch == '\n' || ch == '\r' || ch == '\t')
    {
        Some(text)
    } else {
        None
    }
}

fn binary_decode(value: &str) -> Option<String> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.len() < 4 {
        return None;
    }
    if !tokens
        .iter()
        .all(|token| token.len() % 8 == 0 && token.chars().all(|ch| ch == '0' || ch == '1'))
    {
        return None;
    }
    let mut bytes = Vec::new();
    for token in tokens {
        for chunk in token.as_bytes().chunks(8) {
            let chunk = std::str::from_utf8(chunk).ok()?;
            bytes.push(u8::from_str_radix(chunk, 2).ok()?);
        }
    }
    let text = String::from_utf8(bytes).ok()?;
    if text
        .chars()
        .all(|ch| !ch.is_control() || ch == '\n' || ch == '\r' || ch == '\t')
    {
        Some(text)
    } else {
        None
    }
}

/// Single-byte XOR: return the key whose output is the most readable, with a
/// strong bonus for keying that surfaces a flag-shaped string.
fn xor_single(value: &str) -> Option<(String, String)> {
    let bytes = value.as_bytes();
    if bytes.len() < 8 {
        return None;
    }
    let mut best: Option<(u8, f64, String)> = None;
    for key in 1..=255u8 {
        let decoded: Vec<u8> = bytes.iter().map(|byte| byte ^ key).collect();
        let Ok(text) = String::from_utf8(decoded) else {
            continue;
        };
        let printable = text
            .chars()
            .filter(|ch| !ch.is_control() || *ch == '\n')
            .count();
        let mut score = printable as f64 / text.chars().count().max(1) as f64;
        if score <= 0.9 {
            continue;
        }
        if flag_candidate(&text).is_some() {
            score += 1.0;
        }
        let letters = text.chars().filter(|ch| ch.is_ascii_alphabetic()).count() as f64
            / text.chars().count().max(1) as f64;
        score += letters * 0.25;
        if best
            .as_ref()
            .is_none_or(|(_, best_score, _)| score > *best_score)
        {
            best = Some((key, score, text));
        }
    }
    best.map(|(key, _, text)| (format!("xor(key=0x{key:02x})"), text))
}

fn morse_decode(value: &str) -> Option<String> {
    const LETTERS: [(&str, char); 26] = [
        (".-", 'a'),
        ("-...", 'b'),
        ("-.-.", 'c'),
        ("-..", 'd'),
        (".", 'e'),
        ("..-.", 'f'),
        ("--.", 'g'),
        ("....", 'h'),
        ("..", 'i'),
        (".---", 'j'),
        ("-.-", 'k'),
        (".-..", 'l'),
        ("--", 'm'),
        ("-.", 'n'),
        ("---", 'o'),
        (".--.", 'p'),
        ("--.-", 'q'),
        (".-.", 'r'),
        ("...", 's'),
        ("-", 't'),
        ("..-", 'u'),
        ("...-", 'v'),
        (".--", 'w'),
        ("-..-", 'x'),
        ("-.--", 'y'),
        ("--..", 'z'),
    ];
    let mut out = String::new();
    for word in value.trim().split('/') {
        for letter in word.split_whitespace() {
            match LETTERS.iter().find(|(code, _)| *code == letter) {
                Some((_, decoded)) => out.push(*decoded),
                None => return None,
            }
        }
        out.push(' ');
    }
    Some(out.trim().to_string())
}

/// System-tool decompression for the file-level pass: gzip, bzip2, xz, zlib
/// (the last through python3). Missing tools degrade to a note, never an
/// error.
async fn decompress(bytes: &[u8], root: &Path) -> Result<Option<(String, String)>> {
    let mut attempts: Vec<(String, String)> = Vec::new();
    if bytes.starts_with(&[0x1f, 0x8b]) {
        attempts.push(("gzip".into(), "gzip -dc".into()));
    } else if bytes.starts_with(b"BZh") {
        attempts.push(("bzip2".into(), "bzip2 -dc".into()));
    } else if bytes.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        attempts.push(("xz".into(), "xz -dc".into()));
    } else if matches!(bytes.first(), Some(0x78)) {
        attempts.push((
            "zlib".into(),
            "python3 -c \"import sys,zlib;sys.stdout.buffer.write(zlib.decompress(sys.stdin.buffer.read()))\""
                .into(),
        ));
    } else {
        return Ok(None);
    }
    for (label, command) in attempts {
        if let Ok(output) = shell::run_with_timeout(&command, root, 5).await {
            if !output.trim().is_empty() {
                return Ok(Some((label, output)));
            }
        }
    }
    Ok(None)
}

/// Flag-shaped strings inside sweep outputs: the usual prefixes plus the
/// generic `word{…}` shape so custom formats still surface.
fn flag_candidate(value: &str) -> Option<String> {
    let strict = regex::Regex::new(
        r"(?i)\b(?:flag|ctf|htb|picoctf|gctf|ductf|wrose|hitcon|defcon|justctf)\s*\{[^\s{}]{1,120}\}",
    )
    .ok()?;
    if let Some(found) = strict.find(value) {
        return Some(found.as_str().to_string());
    }
    let generic =
        regex::Regex::new(r"\b[A-Za-z][A-Za-z0-9_]{2,15}\{[\w!@#$%^&*+=:./?-]{4,80}\}").ok()?;
    generic.find(value).map(|found| found.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(input: &str) -> Vec<(String, String)> {
        decoders(input)
    }

    #[test]
    fn base64_round_trip() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("flag{nested}");
        let found = chain(&encoded);
        assert!(
            found
                .iter()
                .any(|(label, value)| label == "base64" && value == "flag{nested}"),
            "got {found:?}"
        );
    }

    #[test]
    fn nested_base64_of_hex_resolves() {
        let hex = "666c61677b786f72787d";
        let outer = base64::engine::general_purpose::STANDARD.encode(hex);
        let level_one = chain(&outer);
        assert!(level_one
            .iter()
            .any(|(label, value)| label == "base64" && value == hex));
        let level_two = chain(&level_one[0].1);
        assert!(
            level_two
                .iter()
                .any(|(label, value)| label == "hex" && value == "flag{xorx}"),
            "got {level_two:?}"
        );
    }

    #[test]
    fn hex_url_and_rot13_decode() {
        assert_eq!(hex_decode("666c6167").as_deref(), Some("flag"));
        assert_eq!(url_decode("flag%7Bx%7D").as_deref(), Some("flag{x}"));
        assert_eq!(rot_n("synt", 13), "flag");
    }

    #[test]
    fn atbash_and_caesar() {
        assert_eq!(atbash("flgz"), "uota");
        // Recovering rot-7 ciphertext needs the inverse shift, 19.
        assert_eq!(best_caesar(&rot_n("this is a secret message", 7)), Some(19));
    }

    #[test]
    fn base32_base58_ascii85() {
        assert_eq!(base32_decode("MZXW6YTBOI======").as_deref(), Some("foobar"));
        assert_eq!(base58_decode("Cn8eVZg").as_deref(), Some("hello"));
        assert_eq!(
            ascii85_decode("<~87cURD]i,\"Ebo80").as_deref(),
            Some("Hello World!")
        );
    }

    #[test]
    fn xor_recovers_the_key() {
        let plain = b"flag{xor_me_please}";
        let encoded: String = plain.iter().map(|byte| (byte ^ 0x42) as char).collect();
        let (label, decoded) = xor_single(&encoded).expect("xor should recover");
        assert_eq!(label, "xor(key=0x42)");
        assert_eq!(decoded, "flag{xor_me_please}");
    }

    #[test]
    fn morse_and_binary() {
        assert_eq!(
            morse_decode("..-. .-.. .- --. / -.-. .-. ..- -..").as_deref(),
            Some("flag crud")
        );
        assert_eq!(
            binary_decode("01100110 01101100 01100001 01100111").as_deref(),
            Some("flag")
        );
    }

    #[test]
    fn flag_candidates_surface() {
        assert_eq!(
            flag_candidate("the answer is flag{yes} congrats").as_deref(),
            Some("flag{yes}")
        );
        assert_eq!(
            flag_candidate("wrap in picoCTF{abc123}").as_deref(),
            Some("picoCTF{abc123}")
        );
        assert!(flag_candidate("nothing here at all").is_none());
    }
}
