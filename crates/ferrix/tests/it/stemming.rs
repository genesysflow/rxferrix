//! Snowball stemming (`tokenizer = "snowball"`): inflected forms in Romanian, French and German find each other,
//! diacritics are folded or kept, changing the language rebuilds the index, and the tokenizer exists on every
//! connection (readers included).

use ferrix::db::Database;
use ferrix::testing::TestApp;
use ferrix::{App, AppOptions, Table, Tokenizer};

pub mod stem {
    use ferrix::prelude::*;

    #[table(name = "stem_docs")]
    #[search_index(ro, search = body, filter = [lang], tokenizer = "snowball", language = "romanian")]
    #[search_index(ro_keep, search = body, filter = [lang], tokenizer = "snowball", language = "romanian", diacritics = "keep")]
    #[search_index(fr, search = body, filter = [lang], tokenizer = "snowball", language = "french")]
    #[search_index(de, search = body, filter = [lang], tokenizer = "snowball", language = "german")]
    #[search_index(plain, search = body, filter = [lang])]
    pub struct StemDoc {
        pub lang: String,
        pub body: String,
    }

    /// Bodies in `lang` matching `text` in the index named `index`, sorted.
    #[query]
    pub async fn find(ctx: &QueryCtx, index: String, lang: String, text: String) -> Result<Vec<String>> {
        let idx = match index.as_str() {
            "ro" => StemDoc::ro,
            "ro_keep" => StemDoc::ro_keep,
            "fr" => StemDoc::fr,
            "de" => StemDoc::de,
            _ => StemDoc::plain,
        };
        let docs = ctx
            .db
            .query::<StemDoc>()
            .with_search_index(idx, |q| q.search(StemDoc::body, text).eq(StemDoc::lang, lang))
            .collect()
            .await?;
        let mut out: Vec<String> = docs.into_iter().map(|d| d.value.body).collect();
        out.sort();
        Ok(out)
    }

    /// Bodies matching `text` in the Romanian index, most relevant first.
    #[query]
    pub async fn ranked(ctx: &QueryCtx, text: String) -> Result<Vec<String>> {
        let docs = ctx
            .db
            .query::<StemDoc>()
            .with_search_index(StemDoc::ro, |q| q.search(StemDoc::body, text).eq(StemDoc::lang, "ro".to_string()))
            .collect()
            .await?;
        Ok(docs.into_iter().map(|d| d.value.body).collect())
    }

    #[mutation]
    pub async fn add(ctx: &MutationCtx, lang: String, bodies: Vec<String>) -> Result<()> {
        for body in bodies {
            ctx.db.insert(StemDoc { lang: lang.clone(), body }).await?;
        }
        Ok(())
    }
}

use stem::StemDoc;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

async fn find(t: &TestApp, index: &str, lang: &str, text: &str) -> Vec<String> {
    t.query::<stem::find>((index.to_string(), lang.to_string(), text.to_string())).await.unwrap()
}

async fn seed(app: &App) {
    app.mutation::<stem::add>((
        "ro".to_string(),
        s(&["Orașul are case noi", "Un oraș mic", "Caselor le trebuie acoperiș", "Cărțile copilului", "O carte veche", "Țările vecine"]),
    ))
    .await
    .unwrap();
    app.mutation::<stem::add>((
        "fr".to_string(),
        s(&["Elle chantait souvent", "Nous allons chanter", "Les chevaux courent", "Un cheval blanc", "Les écoles ferment", "Déjà vu"]),
    ))
    .await
    .unwrap();
    app.mutation::<stem::add>((
        "de".to_string(),
        s(&["Die Häuser sind alt", "Ein kleines Haus", "Die Kinder spielen", "Das Kind lacht", "Breite Straßen", "Die schönsten Bücher"]),
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn inflected_forms_match_each_other() {
    let t = TestApp::new().await.unwrap();
    seed(t.app()).await;

    // Romanian: definite, plural and genitive forms of oraș, casă, carte, țară.
    let orase = s(&["Orașul are case noi", "Un oraș mic"]);
    for q in ["orașelor", "orașul", "oraș", "orase"] {
        assert_eq!(find(&t, "ro", "ro", q).await, orase, "{q}");
    }
    assert_eq!(find(&t, "ro", "ro", "casă").await, s(&["Caselor le trebuie acoperiș", "Orașul are case noi"]));
    assert_eq!(find(&t, "ro", "ro", "cărți").await, s(&["Cărțile copilului", "O carte veche"]), "folded: cart = cart");
    assert_eq!(find(&t, "ro", "ro", "țară").await, s(&["Țările vecine"]));
    assert_eq!(find(&t, "ro", "ro", "copil casa").await, Vec::<String>::new(), "every term must match");
    // Without stemming, only prefixes match.
    assert_eq!(find(&t, "plain", "ro", "orașelor").await, Vec::<String>::new());
    assert_eq!(find(&t, "plain", "ro", "oraș").await, orase);

    // French: verb forms, irregular plurals, accents.
    let chanter = s(&["Elle chantait souvent", "Nous allons chanter"]);
    assert_eq!(find(&t, "fr", "fr", "chanté").await, chanter);
    assert_eq!(find(&t, "fr", "fr", "chantait").await, chanter);
    assert_eq!(find(&t, "fr", "fr", "chevaux").await, s(&["Les chevaux courent", "Un cheval blanc"]));
    assert_eq!(find(&t, "fr", "fr", "école").await, s(&["Les écoles ferment"]));
    assert_eq!(find(&t, "plain", "fr", "chevaux").await, s(&["Les chevaux courent"]));

    // German: umlaut plurals, ß.
    assert_eq!(find(&t, "de", "de", "Haus").await, s(&["Die Häuser sind alt", "Ein kleines Haus"]));
    assert_eq!(find(&t, "de", "de", "häusern").await, s(&["Die Häuser sind alt", "Ein kleines Haus"]));
    assert_eq!(find(&t, "de", "de", "Kindern").await, s(&["Das Kind lacht", "Die Kinder spielen"]));
    assert_eq!(find(&t, "de", "de", "Strasse").await, s(&["Breite Straßen"]));
    assert_eq!(find(&t, "de", "de", "Buch schön").await, s(&["Die schönsten Bücher"]));

    // The filter still applies.
    assert_eq!(find(&t, "ro", "fr", "oraș").await, Vec::<String>::new());
    assert!(t.app().db().verify_indexes().await.unwrap().is_empty());
}

#[tokio::test]
async fn diacritics_are_folded_or_kept() {
    let t = TestApp::new().await.unwrap();
    seed(t.app()).await;
    t.mutation::<stem::add>(("ro".to_string(), s(&["Şcoala veche (cu sedilă)", "Orașele sunt mari"]))).await.unwrap();
    let orase = s(&["Orașele sunt mari", "Orașul are case noi", "Un oraș mic"]);

    // Folded (the default): unaccented spellings find accented ones and the other way round, also where the
    // stemmer cuts them differently: "orasele" stems to "ora", "orașele" to "oraş" (folded "oras"), so the
    // accented word is also indexed as "ora" (a colocated synonym). Here "orasele" is not the last (prefix) term,
    // so only that synonym can match it.
    assert_eq!(find(&t, "ro", "ro", "orasele").await, orase);
    assert_eq!(find(&t, "ro", "ro", "orașele").await, orase);
    assert_eq!(find(&t, "ro", "ro", "orasele mari").await, s(&["Orașele sunt mari"]));
    assert_eq!(find(&t, "ro", "ro", "orașele mari").await, s(&["Orașele sunt mari"]));
    assert_eq!(find(&t, "ro", "ro", "tarile").await, s(&["Țările vecine"]));
    assert_eq!(find(&t, "ro", "ro", "carti").await, s(&["Cărțile copilului", "O carte veche"]));
    assert_eq!(find(&t, "fr", "fr", "ecoles").await, s(&["Les écoles ferment"]));
    assert_eq!(find(&t, "fr", "fr", "deja").await, s(&["Déjà vu"]));

    // Kept: accents distinguish terms ("carte" and "cărți" are different words now), but Romanian's comma-below and
    // cedilla spellings of ș/ț are the same letter.
    assert_eq!(find(&t, "ro_keep", "ro", "orașele").await, orase);
    assert_eq!(find(&t, "ro_keep", "ro", "orasele mari").await, Vec::<String>::new());
    assert_eq!(find(&t, "ro_keep", "ro", "cărți").await, s(&["Cărțile copilului"]));
    assert_eq!(find(&t, "ro_keep", "ro", "școala").await, s(&["Şcoala veche (cu sedilă)"]));
    assert_eq!(find(&t, "ro", "ro", "scoala").await, s(&["Şcoala veche (cu sedilă)"]));

    // Relevance works as with the other tokenizers: more occurrences first, the same for any spelling.
    t.mutation::<stem::add>(("ro".to_string(), s(&["orașe orașe orașe"]))).await.unwrap();
    let ranked = t.query::<stem::ranked>(("orașe".to_string(),)).await.unwrap();
    assert_eq!(ranked[0], "orașe orașe orașe");
    assert_eq!(t.query::<stem::ranked>(("orase".to_string(),)).await.unwrap(), ranked);
}

#[tokio::test]
async fn changing_the_language_rebuilds_the_index() {
    let dir = tempfile::tempdir().unwrap();
    {
        let app = App::open(dir.path(), AppOptions::default()).await.unwrap();
        seed(&app).await;
    }
    let original = StemDoc::table_def();
    let info = |db: &Database| db.catalog().table("stem_docs").unwrap().search_indexes.iter().find(|i| i.name == "fr").unwrap().clone();
    let ddl = |n: u32| {
        let conn = rusqlite::Connection::open(dir.path().join("data.db")).unwrap();
        conn.query_row(&format!("SELECT sql FROM sqlite_master WHERE name = '_search_{n}'"), [], |r| r.get::<_, String>(0)).unwrap()
    };
    let db = Database::open(dir.path(), std::slice::from_ref(&original), 1).unwrap();
    let id = info(&db).id;
    let n = id - ferrix::db::search::SPECIAL_INDEX_BASE;
    assert!(ddl(n).contains("ferrix_snowball french remove_diacritics 2"), "{}", ddl(n));
    drop(db);

    // The `fr` index becomes German: rebuilt in place (same id), backfilled, consistent.
    let mut german = original.clone();
    german.search_indexes.iter_mut().find(|s| s.name == "fr").unwrap().tokenizer = Tokenizer::snowball("german");
    let db = Database::open(dir.path(), std::slice::from_ref(&german), 1).unwrap();
    assert_eq!(info(&db).id, id);
    assert_eq!(info(&db).tokenizer().unwrap().language.as_deref(), Some("german"));
    assert!(ddl(n).contains("ferrix_snowball german remove_diacritics 2"), "{}", ddl(n));
    assert_eq!(db.verify_indexes().await.unwrap(), Vec::<String>::new());
    // Searched with German stems now: "Häusern" finds "Häuser" through the index once named `fr`.
    let found: i64 = rusqlite::Connection::open(dir.path().join("data.db"))
        .unwrap()
        .query_row(&format!("SELECT count(*) FROM _search_{n} WHERE _search_{n} MATCH '\"häusern\"'"), [], |r| r.get(0))
        .unwrap();
    assert_eq!(found, 2, "Häuser and Haus");
    drop(db);

    // Keeping diacritics is a change too.
    let mut keep = original.clone();
    keep.search_indexes.iter_mut().find(|s| s.name == "fr").unwrap().tokenizer = Tokenizer::snowball("french").keep_diacritics();
    let db = Database::open(dir.path(), std::slice::from_ref(&keep), 1).unwrap();
    assert!(ddl(n).contains("ferrix_snowball french remove_diacritics 0"), "{}", ddl(n));
    assert_eq!(db.verify_indexes().await.unwrap(), Vec::<String>::new());
    drop(db);

    // Unknown languages, and a snowball tokenizer without one, are refused at startup.
    for bad in [Tokenizer::snowball("klingon"), Tokenizer { language: None, ..Tokenizer::snowball("french") }] {
        let mut def = original.clone();
        def.search_indexes[0].tokenizer = bad;
        let err = Database::open(dir.path(), &[def], 1).err().unwrap();
        assert!(format!("{err:#}").contains("unsupported stemming language"), "{err:#}");
    }

    // Back to the code's definition: French again.
    let app = App::open(dir.path(), AppOptions::default()).await.unwrap();
    let found = app.query::<stem::find>(("fr".to_string(), "fr".to_string(), "chanté".to_string())).await.unwrap();
    assert_eq!(found, s(&["Elle chantait souvent", "Nous allons chanter"]));
    assert!(app.db().verify_indexes().await.unwrap().is_empty());
}

/// FTS5 tokenizers are registered per connection. Queries run on the read-only reader connections (and the HNSW and
/// backup code opens its own), so every connection must have it: a connection without it can't even open the table.
#[tokio::test]
async fn every_connection_has_the_tokenizer() {
    let t = TestApp::new().await.unwrap();
    seed(t.app()).await;
    // Queries read through reader connections: many concurrent ones, so several readers are used.
    let mut runs = Vec::new();
    for _ in 0..16 {
        let app = t.app().clone();
        runs.push(tokio::spawn(async move {
            app.query::<stem::find>(("de".to_string(), "de".to_string(), "Häusern".to_string())).await.unwrap()
        }));
    }
    for r in runs {
        assert_eq!(r.await.unwrap(), s(&["Die Häuser sind alt", "Ein kleines Haus"]));
    }
    // A fresh read-only connection, opened like the readers.
    let conn = ferrix::db::storage::open_reader(&t.dir().join("data.db")).unwrap();
    let id = t.app().db().catalog().table("stem_docs").unwrap().search_indexes.iter().find(|i| i.name == "ro").unwrap().id;
    let n = id - ferrix::db::search::SPECIAL_INDEX_BASE;
    let found: i64 = conn
        .query_row(&format!("SELECT count(*) FROM _search_{n} WHERE _search_{n} MATCH '\"orașelor\"'"), [], |r| r.get(0))
        .unwrap();
    assert_eq!(found, 2);
    // Inside a mutation (the writer connection), too.
    t.mutation::<stem::add>(("ro".to_string(), s(&["Orașele mari"]))).await.unwrap();
    assert_eq!(find(&t, "ro", "ro", "oraș").await.len(), 3);
}
