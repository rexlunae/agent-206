use super::*;
use crate::tests::{Words, planted, unique};

fn document(text: String) -> Document {
    Document {
        path: unique("memory/d") + ".md",
        text,
    }
}
fn terms(query: &str) -> Vec<String> {
    query_terms(query)
}
fn collect(text: &str) -> Vec<String> {
    let mut all = Vec::new();
    words(text, |w| all.push(w.to_string()));
    all
}
fn sections(text: &str) -> Vec<Section> {
    super::sections(text, &mut Work::unlimited()).unwrap()
}
fn rank(documents: &[Document], terms: &[String], limit: usize) -> Vec<Hit> {
    let ranking = super::rank(documents, terms, limit, &mut Work::unlimited());
    assert_eq!(ranking.ranked, documents.len());
    ranking.hits
}

#[test]
fn ranking_stops_between_documents_when_its_work_runs_out() {
    let mut words = Words::new(31);
    let term = planted();
    let per_document = 12;
    let documents: Vec<Document> = (0..10)
        .map(|_| document(format!("{term} {}", words.line(per_document - 1))))
        .collect();
    let cost = WORK_PER_LINE + per_document as u64 * WORK_PER_WORD;
    for (budget, ranked) in [
        (3 * cost + cost / 2, 3),
        (3 * cost, 3),
        (3 * cost - 1, 2),
        (0, 0),
    ] {
        let ranking = super::rank(&documents, &terms(&term), 20, &mut Work::new(budget));
        assert_eq!(ranking.ranked, ranked, "budget {budget}");
        // The statistics cover exactly the ranked documents.
        assert_eq!(ranking.hits, rank(&documents[..ranked], &terms(&term), 20));
    }
    // Non-ASCII characters cost more than ASCII ones.
    let wide = [document(format!("{term} {}", "ä".repeat(10)))];
    let ascii = [document(format!("{term} {}", "a".repeat(10)))];
    let budget = WORK_PER_LINE + 2 * WORK_PER_WORD;
    assert_eq!(
        super::rank(&ascii, &terms(&term), 1, &mut Work::new(budget)).ranked,
        1
    );
    assert_eq!(
        super::rank(&wide, &terms(&term), 1, &mut Work::new(budget)).ranked,
        0
    );
    // A huge single line is metered word by word, not trusted whole.
    let line = [document(format!("{term} {}", words.line(10_000)))];
    let ranking = super::rank(&line, &terms(&term), 1, &mut Work::new(100 * WORK_PER_WORD));
    assert_eq!(ranking.ranked, 0);
    assert!(ranking.hits.is_empty());
}

#[test]
fn sections_split_at_headings_and_every_twenty_lines() {
    let mut words = Words::new(3);
    let mut lines = vec![String::new(), format!("# {}", words.word())];
    lines.extend((0..3).map(|_| words.line(4)));
    lines.push(String::new());
    lines.push(format!("## {}", words.word()));
    lines.extend((0..SECTION_LINES + 2).map(|_| words.line(4)));
    lines.push("   ".into());
    let text = lines.join("\r\n");
    let found: Vec<(u32, u32)> = sections(&text)
        .iter()
        .map(|s| (s.first_line, s.last_line))
        .collect();
    let second = 7;
    let third = second + SECTION_LINES as u32;
    assert_eq!(
        found,
        vec![
            (2, 5),
            (second, third - 1),
            (third, second + SECTION_LINES as u32 + 2)
        ]
    );
    // Byte ranges hold exactly those lines, without line endings at the ends.
    for section in sections(&text) {
        let body = &text[section.bytes.clone()];
        let expected =
            lines[section.first_line as usize - 1..section.last_line as usize].join("\r\n");
        assert_eq!(body, expected);
    }
    // A `###` heading or `#tag` does not start a section.
    assert_eq!(sections("a\n### b\n#c\nd").len(), 1);
    assert!(sections("\n \n\t\n").is_empty());
}

#[test]
fn words_are_lowercased_across_scripts() {
    assert_eq!(
        collect("Dark-Mode, ÄPFEL; _snake_case_ x 7 42 -- naïve 日本語"),
        ["dark-mode", "äpfel", "snake_case", "42", "naïve", "日本語"]
    );
    assert_eq!(query_terms("Alpha alpha ALPHA beta"), ["alpha", "beta"]);
    // Non-ASCII separators split words; ASCII joined to letters stays.
    assert_eq!(
        collect("Naïve—CAFÉ、日本語。テスト x—y ö a-—-b İx"),
        ["naïve", "café", "日本語", "テスト", "ö", "i\u{307}x"]
    );
}

/// The words the tokenizer finds are exactly those of a plain
/// `str::split` on characters that are not letters, digits, `-` or `_`.
#[test]
fn the_fast_tokenizer_matches_the_plain_definition() {
    let mut words = Words::new(37);
    let alphabet: Vec<char> = "aZ9-_ .,\n\t—、。éÆЖж日本İ🦀\u{a0}·ª".chars().collect();
    for _ in 0..300 {
        let length = (words.step() % 60) as usize;
        let text: String = (0..length)
            .map(|_| alphabet[(words.step() % alphabet.len() as u64) as usize])
            .collect();
        let expected: Vec<(String, bool)> = text
            .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
            .map(|w| w.trim_matches(['-', '_']))
            .filter(|w| w.len() > 1 || (!w.is_empty() && !w.is_ascii()))
            .map(|w| (w.to_string(), w.is_ascii()))
            .collect();
        let mut found = Vec::new();
        assert!(raw_words(&text, &mut Work::unlimited(), |w, ascii| found
            .push((w.to_string(), ascii))));
        assert_eq!(found, expected, "{text:?}");
    }
}

#[test]
fn letter_ranges_and_the_cache_agree_with_unicode() {
    for &(first, last) in LETTERS {
        for c in first..=last {
            assert!(c.is_alphanumeric(), "{c:?} is in a letter range");
        }
    }
    // Twice, so the second pass answers from a cache full of collisions.
    for _ in 0..2 {
        for c in '\u{80}'..='\u{2FFFF}' {
            assert_eq!(super::alphanumeric(c).0, c.is_alphanumeric(), "{c:?}");
        }
    }
}

#[test]
fn rare_terms_outrank_common_ones() {
    let mut words = Words::new(19);
    let (common, rare) = (planted(), planted());
    let mut documents: Vec<Document> = (0..30)
        .map(|_| document(format!("{} {common} {}", words.line(8), words.line(8))))
        .collect();
    let target = 17;
    documents[target].text = format!("{} {rare} {}", words.line(8), words.line(7));
    // Plenty of the common term elsewhere cannot beat one rare match.
    documents[3].text = format!("{common} {common} {common} {}", words.line(4));
    let hits = rank(&documents, &terms(&format!("{common} {rare}")), 3);
    assert_eq!(hits[0].document, target);
    assert!(hits[0].score > hits[1].score);
    assert_eq!(hits.len(), 3);
}

#[test]
fn more_occurrences_and_shorter_sections_rank_higher() {
    let mut words = Words::new(23);
    let term = planted();
    let filler = |words: &mut Words, n| words.line(n);
    let documents = vec![
        document(format!("{term} {}", filler(&mut words, 9))),
        document(format!("{term} {term} {term} {}", filler(&mut words, 7))),
        document(format!("{term} {}", filler(&mut words, 30))),
        document(filler(&mut words, 10)),
    ];
    let hits = rank(&documents, &terms(&term), 10);
    let order: Vec<usize> = hits.iter().map(|h| h.document).collect();
    // Equal length (10 words): three occurrences beat one; one occurrence
    // in 10 words beats one in 31; a section without the term is absent.
    assert_eq!(order, vec![1, 0, 2]);
}

#[test]
fn every_matching_section_is_ranked_in_order() {
    let mut words = Words::new(29);
    let seeds: Vec<String> = (0..4).map(|_| planted()).collect();
    let mut documents = Vec::new();
    for _ in 0..25 {
        let lines: Vec<String> = (0..45)
            .map(|_| {
                let count = 1 + (words.step() % 9) as usize;
                let mut line = words.line(count);
                if words.step().is_multiple_of(7) {
                    line.push(' ');
                    line.push_str(&seeds[(words.step() % 4) as usize]);
                }
                line
            })
            .collect();
        documents.push(document(lines.join("\n")));
    }
    let query = seeds[..2].join(" ");
    let terms = terms(&query);
    let hits = rank(&documents, &terms, usize::MAX);
    let matching = documents
        .iter()
        .flat_map(|d| sections(&d.text).into_iter().map(move |s| &d.text[s.bytes]))
        .filter(|body| terms.iter().any(|t| collect(body).contains(t)))
        .count();
    assert!(matching > 10, "the generated corpus is too sparse");
    assert_eq!(hits.len(), matching);
    for pair in hits.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        assert!(
            a.score > b.score
                || (a.score == b.score
                    && (a.document, a.section.first_line) < (b.document, b.section.first_line))
        );
    }
    for hit in &hits {
        let body = &documents[hit.document].text[hit.section.bytes.clone()];
        assert!(terms.iter().any(|t| body.contains(t.as_str())));
    }
    let limited = rank(&documents, &terms, 4);
    assert_eq!(limited, hits[..4]);
    // Ranking is a pure function of the corpus.
    assert_eq!(rank(&documents, &terms, usize::MAX), hits);
}

#[test]
fn ties_go_to_the_earlier_document_and_line() {
    let term = planted();
    let same = format!("{term} same words here");
    let documents = vec![
        document(format!("{same}{}{same}", "\n".repeat(SECTION_LINES))),
        document(same.clone()),
    ];
    let hits = rank(&documents, &terms(&term), 10);
    let order: Vec<(usize, u32)> = hits
        .iter()
        .map(|h| (h.document, h.section.first_line))
        .collect();
    assert_eq!(order, vec![(0, 1), (0, 1 + SECTION_LINES as u32), (1, 1)]);
    assert!(rank(&[], &terms(&term), 10).is_empty());
}

#[test]
fn snippets_cut_only_at_character_boundaries() {
    for prefix in ["", "a", "ab"] {
        let text = format!("{prefix}{}", "é語🦀".repeat(64));
        for limit in 0..40 {
            let cut = snippet(&text, limit);
            let body = cut.strip_suffix('…').unwrap();
            assert!(body.len() <= limit && limit - body.len() < 4);
            assert!(text.starts_with(body));
        }
        assert_eq!(snippet(&text, text.len()), text);
    }
}
