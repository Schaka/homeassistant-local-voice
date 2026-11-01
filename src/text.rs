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

/// Character trigrams of the normalized words, with word borders: "dune"
/// becomes "  d", " du", "dun", "une", "ne ".
pub fn trigrams(s: &str) -> std::collections::HashSet<[char; 3]> {
    let words = words(s);
    if words.is_empty() {
        return std::collections::HashSet::new();
    }
    let padded: Vec<char> = format!("  {} ", words.join(" ")).chars().collect();
    padded.windows(3).map(|w| [w[0], w[1], w[2]]).collect()
}

/// Share of common trigrams (Jaccard index) from 0 to 1. It is high for
/// typos and small mishearings ("lord of the rinks", "intersteller").
pub fn similarity(a: &str, b: &str) -> f32 {
    trigram_similarity(&trigrams(a), &trigrams(b))
}

/// [`similarity`] for precomputed trigram sets.
pub fn trigram_similarity(a: &std::collections::HashSet<[char; 3]>, b: &std::collections::HashSet<[char; 3]>) -> f32 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f32 / union as f32
}

/// The consonants of the words in order, without repeats: "Frieren",
/// "freeran" and "fryren" all become "frn". STT often gets vowels of
/// unknown names wrong but keeps the consonants.
pub fn skeleton(s: &str) -> String {
    let mut out = String::new();
    for c in words(s).concat().chars() {
        if "aeiouy".contains(c) {
            continue;
        }
        if !out.ends_with(c) {
            out.push(c);
        }
    }
    out
}

/// True if the query looks like a title, not a description: at most four
/// words and none of the words that start a description.
pub fn title_like(query: &str) -> bool {
    const DESCRIPTIVE: &[&str] = &[
        "about",
        "with",
        "where",
        "who",
        "which",
        "that",
        "movie",
        "film",
        "show",
        "series",
        "anime",
        "documentary",
        "one",
        "episode",
    ];
    let w = words(query);
    w.len() <= 4 && !w.iter().any(|x| DESCRIPTIVE.contains(&x.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeletons_and_title_likeness() {
        assert_eq!(skeleton("Frieren"), "frn");
        assert_eq!(skeleton("freeran"), "frn");
        assert_eq!(skeleton("fryren"), "frn");
        assert_eq!(skeleton("Futurama"), "ftrm");
        assert!(title_like("game of thrones"));
        assert!(title_like("spy famly"));
        assert!(!title_like("anime about a spy who adopts a girl"));
        assert!(!title_like("the one with keanu reeves"));
        assert!(!title_like("a b c d e"));
    }

    #[test]
    fn trigram_similarity_scores() {
        assert!((similarity("Dune", "dune") - 1.0).abs() < 1e-6);
        assert!(similarity("lord of the rinks", "The Lord of the Rings") > 0.5);
        assert!(similarity("intersteller", "Interstellar") > 0.5);
        assert!(similarity("the office", "Office Space") < 0.5);
        assert_eq!(similarity("", ""), 0.0);
    }

    #[test]
    fn spoken_forms() {
        assert_eq!(squashed("Level 1 Techs"), squashed("level one techs"));
        assert_eq!(
            words("Frieren: Beyond Journey's End"),
            ["frieren", "beyond", "journey", "s", "end"]
        );
    }
}
