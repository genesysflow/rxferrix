//! Snowball stemming for full-text search: the FTS5 tokenizer `ferrix_snowball`, behind
//! `#[search_index(.., tokenizer = "snowball", language = "romanian")]`.
//!
//! FTS5 only ships an English stemmer (`porter`). This tokenizer adds the Snowball stemmers of the `rust-stemmers`
//! crate (see [`crate::schema::Tokenizer::SNOWBALL_LANGUAGES`]) the same way `porter` adds its own: it wraps FTS5's
//! `unicode61` tokenizer, so words are split, case-folded and (optionally) stripped of diacritics exactly like the
//! default tokenizer, and only the stemming step is new.
//!
//! ## What a token becomes
//!
//! The text is split by a `unicode61 remove_diacritics 0` instance (words, lowercased, accents kept: the stemmers'
//! suffix rules need them, e.g. Romanian `-ă`, French `-é`). Each word is then normalized for its language (Romanian
//! `ș`/`ț` with a comma below become the cedilla forms `ş`/`ţ` the stemmer knows) and stemmed. With `diacritics =
//! "keep"` that stem is the token. With `diacritics = "remove"` (the default), the token is the stem folded by a second
//! instance, `unicode61 remove_diacritics 2` (so `café` = `cafe` exactly as in the default tokenizer), and, when it
//! differs, the folded stem of the folded word is added as a *colocated* token (an FTS5 synonym at the same
//! position). The same happens to query terms, where FTS5 treats colocated tokens as alternatives. So a word typed
//! without its accents (`casa`, `pere`) still finds the accented forms (`casă`, `père`) even when the stemmer would
//! cut the accented and the unaccented spelling differently. Colocated tokens don't count toward the document length,
//! and a phrase matches once per position, so `ferrix_score` sees the same occurrences and lengths as without them.
//!
//! The last (prefix) term of a query is stemmed too, like `porter` does: a complete word typed last still finds its
//! inflected forms.
//!
//! ## Registration
//!
//! FTS5 tokenizers are per connection. [`register`] is called from the auto extension that also registers
//! `ferrix_score` ([`super::score`]), i.e. for every SQLite connection the process opens after
//! [`super::register_extensions`], the writer, the read-only readers and the ones opened by backups and exports.
//! A connection without it could not even open an FTS5 table that uses it ("no such tokenizer").
//!
//! ## Safety
//!
//! All the FFI is in this file. Every pointer it dereferences comes from FTS5 (or from this file, for the tokenizer
//! object), and FTS5 guarantees their lifetimes: the `fts5_api` pointer registered as user data lives as long as the
//! connection, a tokenizer object lives between its `xCreate` and `xDelete`, and the text and callback context of
//! `xTokenize` are valid during that call. The tokenizer object is a `Box<Snowball>` turned into a raw pointer by
//! `xCreate` and back into a `Box` by `xDelete` (FTS5 calls it exactly once). FTS5 never uses one tokenizer object
//! from two threads at once (it belongs to one table of one connection). No panic crosses the FFI boundary: every
//! entry point runs its Rust code under `catch_unwind` and reports a panic as `SQLITE_ERROR`.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use rusqlite::ffi;
use rust_stemmers::{Algorithm, Stemmer};

/// The FTS5 tokenizer name: `tokenize = 'ferrix_snowball <language> remove_diacritics <0|2>'`.
pub const TOKENIZER: &str = "ferrix_snowball";

/// The Snowball algorithm of a language name (`crate::schema::Tokenizer::SNOWBALL_LANGUAGES`).
pub fn algorithm(language: &str) -> Option<Algorithm> {
    Some(match language {
        "arabic" => Algorithm::Arabic,
        "danish" => Algorithm::Danish,
        "dutch" => Algorithm::Dutch,
        "english" => Algorithm::English,
        "finnish" => Algorithm::Finnish,
        "french" => Algorithm::French,
        "german" => Algorithm::German,
        "greek" => Algorithm::Greek,
        "hungarian" => Algorithm::Hungarian,
        "italian" => Algorithm::Italian,
        "norwegian" => Algorithm::Norwegian,
        "portuguese" => Algorithm::Portuguese,
        "romanian" => Algorithm::Romanian,
        "russian" => Algorithm::Russian,
        "spanish" => Algorithm::Spanish,
        "swedish" => Algorithm::Swedish,
        "tamil" => Algorithm::Tamil,
        "turkish" => Algorithm::Turkish,
        _ => return None,
    })
}

/// Normalize a lowercased word for its stemmer: the Romanian stemmer only knows `ş`/`ţ` with a cedilla, while
/// Romanian text normally uses `ș`/`ț` with a comma below.
fn normalize(algorithm: Algorithm, word: &str) -> std::borrow::Cow<'_, str> {
    if algorithm == Algorithm::Romanian && word.contains(['ș', 'ț', 'Ș', 'Ț']) {
        word.chars()
            .map(|c| match c {
                'ș' | 'Ș' => 'ş',
                'ț' | 'Ț' => 'ţ',
                c => c,
            })
            .collect::<String>()
            .into()
    } else {
        word.into()
    }
}

/// The token callback FTS5 hands to `xTokenize`.
type TokenCallback = unsafe extern "C" fn(*mut c_void, c_int, *const c_char, c_int, c_int, c_int) -> c_int;

/// An instance of another tokenizer (`unicode61`), created through `xFindTokenizer`, deleted on drop.
struct Parent {
    methods: ffi::fts5_tokenizer,
    instance: *mut ffi::Fts5Tokenizer,
}

impl Parent {
    /// Create an instance of the tokenizer `name` with `args`.
    ///
    /// # Safety
    /// `api` must be the connection's valid `fts5_api` pointer.
    unsafe fn create(api: *mut ffi::fts5_api, name: &CStr, args: &[&CStr]) -> Result<Parent, c_int> {
        // SAFETY: `api` is valid (caller); `xFindTokenizer` fills `methods` and `user_data` and returns a status.
        let (methods, user_data) = unsafe {
            let find = (*api).xFindTokenizer.ok_or(ffi::SQLITE_ERROR)?;
            let mut methods: ffi::fts5_tokenizer = std::mem::zeroed();
            let mut user_data: *mut c_void = ptr::null_mut();
            let rc = find(api, name.as_ptr(), &mut user_data, &mut methods);
            if rc != ffi::SQLITE_OK {
                return Err(rc);
            }
            (methods, user_data)
        };
        let create = methods.xCreate.ok_or(ffi::SQLITE_ERROR)?;
        if methods.xDelete.is_none() || methods.xTokenize.is_none() {
            return Err(ffi::SQLITE_ERROR);
        }
        let mut argv: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();
        let mut instance = ptr::null_mut();
        // SAFETY: `create` is the tokenizer's constructor with its own user data; `argv` holds `args.len()` valid C
        // strings that outlive the call (FTS5 tokenizers copy what they keep).
        let rc = unsafe { create(user_data, argv.as_mut_ptr(), argv.len() as c_int, &mut instance) };
        if rc != ffi::SQLITE_OK || instance.is_null() {
            return Err(if rc == ffi::SQLITE_OK { ffi::SQLITE_ERROR } else { rc });
        }
        Ok(Parent { methods, instance })
    }

    /// Tokenize `text`, calling `callback(ctx, ..)` for every token.
    ///
    /// # Safety
    /// `ctx` must be what `callback` expects, valid for the whole call.
    unsafe fn tokenize(&self, ctx: *mut c_void, flags: c_int, text: &[u8], callback: TokenCallback) -> c_int {
        let tokenize = self.methods.xTokenize.expect("checked in create");
        // SAFETY: a live instance of this tokenizer (dropped only with `self`); `text` is valid for `text.len()`
        // bytes during the call; `ctx` matches `callback` (caller).
        unsafe { tokenize(self.instance, ctx, flags, text.as_ptr().cast(), text.len() as c_int, Some(callback)) }
    }
}

impl Drop for Parent {
    fn drop(&mut self) {
        let delete = self.methods.xDelete.expect("checked in create");
        // SAFETY: the instance was created by this tokenizer's `xCreate` and is deleted exactly once.
        unsafe { delete(self.instance) }
    }
}

/// One `ferrix_snowball` tokenizer instance (one per FTS5 table and connection).
struct Snowball {
    algorithm: Algorithm,
    stemmer: Stemmer,
    /// `unicode61 remove_diacritics 0`: splits the text into lowercased words, accents kept.
    words: Parent,
    /// `unicode61 remove_diacritics 2`, to fold one stem at a time; `None` with `diacritics = "keep"`.
    fold: Option<Parent>,
}

impl Snowball {
    /// The tokens of one word: the stem (folded unless diacritics are kept) and, when it differs, the folded stem of
    /// the folded word (a colocated synonym).
    fn tokens(&self, word: &str, flags: c_int) -> Result<(String, Option<String>), c_int> {
        let stem = |w: &str| self.stemmer.stem(&normalize(self.algorithm, w)).into_owned();
        let Some(fold) = &self.fold else { return Ok((stem(word), None)) };
        let fold = |w: &str| fold_word(fold, w, flags);
        let first = fold(&stem(word))?;
        let folded_word = fold(word)?;
        let second = if folded_word == word { first.clone() } else { fold(&stem(&folded_word))? };
        Ok(if second == first { (first, None) } else { (first, Some(second)) })
    }
}

/// `word` as the folding tokenizer returns it (its tokens concatenated; one token for a single word).
fn fold_word(fold: &Parent, word: &str, flags: c_int) -> Result<String, c_int> {
    unsafe extern "C" fn collect(
        ctx: *mut c_void,
        _flags: c_int,
        token: *const c_char,
        len: c_int,
        _start: c_int,
        _end: c_int,
    ) -> c_int {
        // SAFETY: `ctx` is the `Vec<u8>` passed below; FTS5 passes a token of `len` bytes valid during this call.
        unsafe {
            let out = &mut *ctx.cast::<Vec<u8>>();
            if len > 0 && !token.is_null() {
                out.extend_from_slice(std::slice::from_raw_parts(token.cast::<u8>(), len as usize));
            }
        }
        ffi::SQLITE_OK
    }
    let mut out: Vec<u8> = Vec::with_capacity(word.len());
    // SAFETY: `collect` expects a `*mut Vec<u8>`, which `out` is for the whole call.
    let rc = unsafe { fold.tokenize((&mut out as *mut Vec<u8>).cast(), flags, word.as_bytes(), collect) };
    if rc != ffi::SQLITE_OK {
        return Err(rc);
    }
    String::from_utf8(out).map_err(|_| ffi::SQLITE_ERROR)
}

/// Parse `<language> [remove_diacritics <0|1|2>]`.
fn parse_args(args: &[&str]) -> Option<(Algorithm, bool)> {
    match args {
        [language] => Some((algorithm(language)?, true)),
        [language, "remove_diacritics", n] => {
            let remove = match *n {
                "0" => false,
                "1" | "2" => true,
                _ => return None,
            };
            Some((algorithm(language)?, remove))
        }
        _ => None,
    }
}

/// `xCreate`: `p_ctx` is the connection's `fts5_api` (registered as the tokenizer's user data).
unsafe extern "C" fn x_create(
    p_ctx: *mut c_void,
    az_arg: *mut *const c_char,
    n_arg: c_int,
    pp_out: *mut *mut ffi::Fts5Tokenizer,
) -> c_int {
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<Box<Snowball>, c_int> {
        let api = p_ctx.cast::<ffi::fts5_api>();
        if api.is_null() || (n_arg > 0 && az_arg.is_null()) {
            return Err(ffi::SQLITE_ERROR);
        }
        // SAFETY: FTS5 passes `n_arg` NUL-terminated argument strings, valid during this call.
        let args: Vec<&str> = (0..n_arg.max(0) as usize)
            .map(|i| unsafe { CStr::from_ptr(*az_arg.add(i)) }.to_str().map_err(|_| ffi::SQLITE_ERROR))
            .collect::<Result<_, _>>()?;
        let (algorithm, remove_diacritics) = parse_args(&args).ok_or(ffi::SQLITE_ERROR)?;
        // SAFETY: `api` is the connection's `fts5_api` (see `register`).
        let words = unsafe { Parent::create(api, c"unicode61", &[c"remove_diacritics", c"0"])? };
        let fold = if remove_diacritics {
            // SAFETY: as above.
            Some(unsafe { Parent::create(api, c"unicode61", &[c"remove_diacritics", c"2"])? })
        } else {
            None
        };
        Ok(Box::new(Snowball { algorithm, stemmer: Stemmer::create(algorithm), words, fold }))
    }));
    match result {
        Ok(Ok(tokenizer)) => {
            // SAFETY: `pp_out` is FTS5's out-pointer. Ownership of the box moves to FTS5 until `x_delete`.
            unsafe { *pp_out = Box::into_raw(tokenizer).cast() };
            ffi::SQLITE_OK
        }
        Ok(Err(rc)) => rc,
        Err(_) => ffi::SQLITE_ERROR,
    }
}

/// `xDelete`: frees a tokenizer created by [`x_create`] (and its two `unicode61` instances).
unsafe extern "C" fn x_delete(tokenizer: *mut ffi::Fts5Tokenizer) {
    if tokenizer.is_null() {
        return;
    }
    // SAFETY: created by `Box::into_raw` in `x_create`; FTS5 deletes each tokenizer exactly once.
    let tokenizer = unsafe { Box::from_raw(tokenizer.cast::<Snowball>()) };
    let _ = catch_unwind(AssertUnwindSafe(move || drop(tokenizer)));
}

/// The state of one `xTokenize` call, passed through the parent tokenizer to [`on_word`].
struct Call<'a> {
    tokenizer: &'a Snowball,
    ctx: *mut c_void,
    callback: TokenCallback,
    flags: c_int,
}

/// `xTokenize`: split with the parent, stem every word, emit the tokens.
unsafe extern "C" fn x_tokenize(
    tokenizer: *mut ffi::Fts5Tokenizer,
    ctx: *mut c_void,
    flags: c_int,
    text: *const c_char,
    n_text: c_int,
    callback: Option<TokenCallback>,
) -> c_int {
    let Some(callback) = callback else { return ffi::SQLITE_ERROR };
    if tokenizer.is_null() || n_text < 0 || (n_text > 0 && text.is_null()) {
        return ffi::SQLITE_ERROR;
    }
    // SAFETY: a live tokenizer created by `x_create`; only FTS5's connection thread uses it.
    let tokenizer = unsafe { &*tokenizer.cast::<Snowball>() };
    let text: &[u8] = if n_text == 0 {
        &[]
    } else {
        // SAFETY: FTS5 passes `n_text` valid bytes, valid during this call.
        unsafe { std::slice::from_raw_parts(text.cast::<u8>(), n_text as usize) }
    };
    let mut call = Call { tokenizer, ctx, callback, flags };
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `on_word` expects a `*mut Call`, which `call` is for the whole call.
        unsafe { tokenizer.words.tokenize((&mut call as *mut Call).cast(), flags, text, on_word) }
    }));
    result.unwrap_or(ffi::SQLITE_ERROR)
}

/// The parent's token callback: stem the word and pass the result (with the original offsets) to FTS5.
unsafe extern "C" fn on_word(
    state: *mut c_void,
    tflags: c_int,
    token: *const c_char,
    len: c_int,
    start: c_int,
    end: c_int,
) -> c_int {
    // SAFETY: `state` is the `Call` of the `x_tokenize` running this parent tokenizer.
    let call = unsafe { &*state.cast::<Call>() };
    let bytes: &[u8] = if len <= 0 || token.is_null() {
        &[]
    } else {
        // SAFETY: the parent passes a token of `len` bytes, valid during this callback.
        unsafe { std::slice::from_raw_parts(token.cast::<u8>(), len as usize) }
    };
    let emit = |flags: c_int, t: &[u8]| -> c_int {
        // SAFETY: FTS5's own callback and context for this `xTokenize`; `t` is valid during the call.
        unsafe { (call.callback)(call.ctx, flags, t.as_ptr().cast(), t.len() as c_int, start, end) }
    };
    let Ok(word) = std::str::from_utf8(bytes) else { return emit(tflags, bytes) };
    let tokens = catch_unwind(AssertUnwindSafe(|| call.tokenizer.tokens(word, call.flags)));
    let (first, second) = match tokens {
        Ok(Ok(t)) => t,
        Ok(Err(rc)) => return rc,
        Err(_) => return ffi::SQLITE_ERROR,
    };
    // An empty stem (never for real words) would drop the word; keep the word itself instead.
    let first = if first.is_empty() { word.to_string() } else { first };
    let rc = emit(tflags, first.as_bytes());
    if rc != ffi::SQLITE_OK {
        return rc;
    }
    match second {
        Some(s) if !s.is_empty() => emit(tflags | ffi::FTS5_TOKEN_COLOCATED, s.as_bytes()),
        _ => ffi::SQLITE_OK,
    }
}

/// Register `ferrix_snowball` with a connection's FTS5. Called by the auto extension in [`super::score`] for every
/// connection.
///
/// # Safety
/// `api` must be the valid `fts5_api` pointer of the connection being opened.
pub(super) unsafe fn register(api: *mut ffi::fts5_api) -> c_int {
    // SAFETY: `api` is valid (caller).
    let Some(create) = (unsafe { (*api).xCreateTokenizer }) else { return ffi::SQLITE_ERROR };
    let mut methods = ffi::fts5_tokenizer { xCreate: Some(x_create), xDelete: Some(x_delete), xTokenize: Some(x_tokenize) };
    let name = c"ferrix_snowball";
    debug_assert_eq!(name.to_str(), Ok(TOKENIZER));
    // SAFETY: FTS5 copies `methods`; the user data is the `fts5_api` itself, which lives as long as the connection
    // and every tokenizer created on it, so it needs no destructor.
    unsafe { create(api, name.as_ptr(), api.cast(), &mut methods, None) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Tokenizer;

    #[test]
    fn every_language_has_a_stemmer() {
        for l in Tokenizer::SNOWBALL_LANGUAGES {
            assert!(algorithm(l).is_some(), "{l}");
        }
        assert!(algorithm("klingon").is_none());
        assert_eq!(parse_args(&["french"]).map(|a| a.1), Some(true));
        assert_eq!(parse_args(&["french", "remove_diacritics", "0"]).map(|a| a.1), Some(false));
        assert!(parse_args(&["french", "remove_diacritics", "x"]).is_none());
        assert!(parse_args(&[]).is_none());
    }

    /// The tokens FTS5 indexes for `text` (through `fts5vocab`), per tokenizer arguments.
    fn vocabulary(args: &str, text: &str) -> Vec<String> {
        super::super::register_extensions();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE f USING fts5(t, tokenize = '{TOKENIZER} {args}');
             CREATE VIRTUAL TABLE v USING fts5vocab(f, row);"
        ))
        .unwrap();
        conn.execute("INSERT INTO f (t) VALUES (?1)", [text]).unwrap();
        conn.prepare("SELECT term FROM v ORDER BY term").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    }

    #[test]
    fn stems_and_folds() {
        assert_eq!(vocabulary("german remove_diacritics 0", "Häuser Haus"), ["haus"]);
        assert_eq!(vocabulary("french remove_diacritics 0", "chantait chanter chanté"), ["chant"]);
        // Kept diacritics: `ş` (both spellings) stays; removed: plain `s`.
        assert_eq!(vocabulary("romanian remove_diacritics 0", "școala şcoala"), ["şcoal"]);
        assert_eq!(vocabulary("romanian remove_diacritics 2", "școala"), ["scoal"]);
        // Unknown languages and arguments are errors when the table is created.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        for bad in ["klingon", "french remove_diacritics 9", "french extra"] {
            assert!(conn.execute_batch(&format!("CREATE VIRTUAL TABLE b USING fts5(t, tokenize = '{TOKENIZER} {bad}')")).is_err());
        }
    }

    /// Words the stemmer cuts differently with and without accents get a colocated synonym.
    #[test]
    fn unaccented_spellings_are_colocated_synonyms() {
        // "orașele" stems to "oraş" (folded "oras"), its unaccented spelling "orasele" to "ora".
        assert_eq!(vocabulary("romanian remove_diacritics 2", "orașele"), ["ora", "oras"]);
        assert_eq!(vocabulary("french remove_diacritics 2", "déjà"), ["dej", "deja"]);
        assert_eq!(vocabulary("romanian remove_diacritics 0", "orașele"), ["oraş"], "no synonyms when diacritics are kept");
    }

    /// Colocated synonyms neither add occurrences nor length: `ferrix_score` is the same as for plain tokens.
    #[test]
    fn scores_ignore_colocated_synonyms() {
        use super::super::score::score;
        super::super::register_extensions();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE f USING fts5(t, tokenize = '{TOKENIZER} romanian remove_diacritics 2');
             INSERT INTO f (rowid, t) VALUES (1, 'orașele orașele'), (2, 'orașele mari și case'), (3, 'nimic');"
        ))
        .unwrap();
        for query in ["\"orașele\"", "\"orasele\"", "\"orașul\"", "\"orașele\" *", "\"ora\" *", "\"orasele\" *"] {
            let rows: Vec<(i64, f64)> = conn
                .prepare("SELECT rowid, ferrix_score(f) AS sc FROM f WHERE f MATCH ?1 ORDER BY sc DESC, rowid")
                .unwrap()
                .query_map([query], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(rows, vec![(1, score(&[2], 2)), (2, score(&[1], 4))], "{query}");
        }
    }
}
