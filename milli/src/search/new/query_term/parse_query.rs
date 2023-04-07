use charabia::{normalizer::NormalizedTokenIter, SeparatorKind, TokenKind};

use crate::{Result, SearchContext, MAX_WORD_LENGTH};

use super::*;

/// Convert the tokenised search query into a list of located query terms.
// TODO: checking if the positions are correct for phrases, separators, ngrams
pub fn located_query_terms_from_string(
    ctx: &mut SearchContext,
    query: NormalizedTokenIter<&[u8]>,
    words_limit: Option<usize>,
) -> Result<Vec<LocatedQueryTerm>> {
    let nbr_typos = number_of_typos_allowed(ctx)?;

    let mut located_terms = Vec::new();

    let mut phrase: Option<PhraseBuilder> = None;

    let parts_limit = words_limit.unwrap_or(usize::MAX);

    // start with the last position as we will wrap around to position 0 at the beginning of the loop below.
    let mut position = u16::MAX;

    let mut peekable = query.take(super::limits::MAX_TOKEN_COUNT).peekable();
    while let Some(token) = peekable.next() {
        if token.lemma().is_empty() {
            continue;
        }
        // early return if word limit is exceeded
        if located_terms.len() >= parts_limit {
            return Ok(located_terms);
        }

        match token.kind {
            TokenKind::Word | TokenKind::StopWord => {
                // On first loop, goes from u16::MAX to 0, then normal increment.
                position = position.wrapping_add(1);

                // 1. if the word is quoted we push it in a phrase-buffer waiting for the ending quote,
                // 2. if the word is not the last token of the query and is not a stop_word we push it as a non-prefix word,
                // 3. if the word is the last token of the query we push it as a prefix word.
                if let Some(phrase) = &mut phrase {
                    phrase.push_word(ctx, &token, position)
                } else if peekable.peek().is_some() {
                    match token.kind {
                        TokenKind::Word => {
                            let word = token.lemma();
                            let term = partially_initialized_term_from_word(
                                ctx,
                                word,
                                nbr_typos(word),
                                false,
                            )?;
                            let located_term = LocatedQueryTerm {
                                value: ctx.term_interner.push(term),
                                positions: position..=position,
                            };
                            located_terms.push(located_term);
                        }
                        TokenKind::StopWord | TokenKind::Separator(_) | TokenKind::Unknown => {}
                    }
                } else {
                    let word = token.lemma();
                    let term =
                        partially_initialized_term_from_word(ctx, word, nbr_typos(word), true)?;
                    let located_term = LocatedQueryTerm {
                        value: ctx.term_interner.push(term),
                        positions: position..=position,
                    };
                    located_terms.push(located_term);
                }
            }
            TokenKind::Separator(separator_kind) => {
                match separator_kind {
                    SeparatorKind::Hard => {
                        position += 1;
                    }
                    SeparatorKind::Soft => {
                        position += 0;
                    }
                }

                phrase = 'phrase: {
                    let phrase = phrase.take();

                    // If we have a hard separator inside a phrase, we immediately start a new phrase
                    let phrase = if separator_kind == SeparatorKind::Hard {
                        if let Some(phrase) = phrase {
                            if let Some(located_query_term) = phrase.build(ctx) {
                                located_terms.push(located_query_term)
                            }
                            Some(PhraseBuilder::empty())
                        } else {
                            None
                        }
                    } else {
                        phrase
                    };

                    // We close and start a new phrase depending on the number of double quotes
                    let mut quote_count = token.lemma().chars().filter(|&s| s == '"').count();
                    if quote_count == 0 {
                        break 'phrase phrase;
                    }

                    // Consume the closing quote and the phrase
                    if let Some(phrase) = phrase {
                        // Per the check above, quote_count > 0
                        quote_count -= 1;
                        if let Some(located_query_term) = phrase.build(ctx) {
                            located_terms.push(located_query_term)
                        }
                    }

                    // Start new phrase if the token ends with an opening quote
                    (quote_count % 2 == 1).then_some(PhraseBuilder::empty())
                };
            }
            _ => (),
        }
    }

    // If a quote is never closed, we consider all of the end of the query as a phrase.
    if let Some(phrase) = phrase.take() {
        if let Some(located_query_term) = phrase.build(ctx) {
            located_terms.push(located_query_term);
        }
    }

    Ok(located_terms)
}

pub fn number_of_typos_allowed<'ctx>(
    ctx: &SearchContext<'ctx>,
) -> Result<impl Fn(&str) -> u8 + 'ctx> {
    let authorize_typos = ctx.index.authorize_typos(ctx.txn)?;
    let min_len_one_typo = ctx.index.min_word_len_one_typo(ctx.txn)?;
    let min_len_two_typos = ctx.index.min_word_len_two_typos(ctx.txn)?;

    // TODO: should `exact_words` also disable prefix search, ngrams, split words, or synonyms?
    let exact_words = ctx.index.exact_words(ctx.txn)?;

    Ok(Box::new(move |word: &str| {
        if !authorize_typos
            || word.len() < min_len_one_typo as usize
            || exact_words.as_ref().map_or(false, |fst| fst.contains(word))
        {
            0
        } else if word.len() < min_len_two_typos as usize {
            1
        } else {
            2
        }
    }))
}

pub fn make_ngram(
    ctx: &mut SearchContext,
    terms: &[LocatedQueryTerm],
    number_of_typos_allowed: &impl Fn(&str) -> u8,
) -> Result<Option<LocatedQueryTerm>> {
    assert!(!terms.is_empty());
    for t in terms {
        if ctx.term_interner.get(t.value).zero_typo.phrase.is_some() {
            return Ok(None);
        }
    }
    for ts in terms.windows(2) {
        let [t1, t2] = ts else { panic!() };
        if *t1.positions.end() != t2.positions.start() - 1 {
            return Ok(None);
        }
    }
    let mut words_interned = vec![];
    for term in terms {
        if let Some(original_term_word) = term.value.original_single_word(ctx) {
            words_interned.push(original_term_word);
        } else {
            return Ok(None);
        }
    }
    let words =
        words_interned.iter().map(|&i| ctx.word_interner.get(i).to_owned()).collect::<Vec<_>>();

    let start = *terms.first().as_ref().unwrap().positions.start();
    let end = *terms.last().as_ref().unwrap().positions.end();
    let is_prefix = ctx.term_interner.get(terms.last().as_ref().unwrap().value).is_prefix;
    let ngram_str = words.join("");
    if ngram_str.len() > MAX_WORD_LENGTH {
        return Ok(None);
    }
    let ngram_str_interned = ctx.word_interner.insert(ngram_str.clone());

    let max_nbr_typos =
        number_of_typos_allowed(ngram_str.as_str()).saturating_sub(terms.len() as u8 - 1);

    let mut term = partially_initialized_term_from_word(ctx, &ngram_str, max_nbr_typos, is_prefix)?;

    // Now add the synonyms
    let index_synonyms = ctx.index.synonyms(ctx.txn)?;

    term.zero_typo.synonyms.extend(
        index_synonyms.get(&words).cloned().unwrap_or_default().into_iter().map(|words| {
            let words = words.into_iter().map(|w| Some(ctx.word_interner.insert(w))).collect();
            ctx.phrase_interner.insert(Phrase { words })
        }),
    );

    let term = QueryTerm {
        original: ngram_str_interned,
        ngram_words: Some(words_interned),
        is_prefix,
        max_nbr_typos,
        zero_typo: term.zero_typo,
        one_typo: Lazy::Uninit,
        two_typo: Lazy::Uninit,
    };

    let term = LocatedQueryTerm { value: ctx.term_interner.push(term), positions: start..=end };

    Ok(Some(term))
}

struct PhraseBuilder {
    words: Vec<Option<Interned<String>>>,
    start: u16,
    end: u16,
}

impl PhraseBuilder {
    fn empty() -> Self {
        Self { words: Default::default(), start: u16::MAX, end: u16::MAX }
    }

    fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    // precondition: token has kind Word or StopWord
    fn push_word(&mut self, ctx: &mut SearchContext, token: &charabia::Token, position: u16) {
        if self.is_empty() {
            self.start = position;
        }
        self.end = position;
        if let TokenKind::StopWord = token.kind {
            self.words.push(None);
        } else {
            // token has kind Word
            let word = ctx.word_interner.insert(token.lemma().to_string());
            // TODO: in a phrase, check that every word exists
            // otherwise return an empty term
            self.words.push(Some(word));
        }
    }

    fn build(self, ctx: &mut SearchContext) -> Option<LocatedQueryTerm> {
        if self.is_empty() {
            return None;
        }
        Some(LocatedQueryTerm {
            value: ctx.term_interner.push({
                let phrase = ctx.phrase_interner.insert(Phrase { words: self.words });
                let phrase_desc = phrase.description(ctx);
                QueryTerm {
                    original: ctx.word_interner.insert(phrase_desc),
                    ngram_words: None,
                    max_nbr_typos: 0,
                    is_prefix: false,
                    zero_typo: ZeroTypoTerm {
                        phrase: Some(phrase),
                        exact: None,
                        prefix_of: BTreeSet::default(),
                        synonyms: BTreeSet::default(),
                        use_prefix_db: None,
                    },
                    one_typo: Lazy::Uninit,
                    two_typo: Lazy::Uninit,
                }
            }),
            positions: self.start..=self.end,
        })
    }
}
