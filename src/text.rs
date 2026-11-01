//! Matching of spoken names against titles ("level one techs" and
//! "Level1Techs", "spy family" and "Spy x Family").

/// Spoken numbers as digits.
const NUMBER_WORDS: &[(&str, &str)] = &[
    ("zero", "0"),
    ("one", "1"),
    ("two", "2"),
    ("three", "3"),
    ("four", "4"),
    ("five", "5"),
    ("six", "6"),
    ("seven", "7"),
    ("eight", "8"),
    ("nine", "9"),
    ("ten", "10"),
];

/// Lowercase words of letters and digits, with spoken numbers as digits.
pub fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| {
            NUMBER_WORDS
                .iter()
                .find(|(word, _)| *word == w)
                .map_or(w, |(_, digit)| digit)
                .to_owned()
        })
        .collect()
}

/// All words in one string: "Level 1 Techs" and "level one techs" both become "level1techs".
pub fn squashed(s: &str) -> String {
    words(s).concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spoken_forms() {
        assert_eq!(squashed("Level 1 Techs"), squashed("level one techs"));
        assert_eq!(
            words("Frieren: Beyond Journey's End"),
            ["frieren", "beyond", "journey", "s", "end"]
        );
    }
}
