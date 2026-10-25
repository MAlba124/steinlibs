//! Generated documents shaped like the traffic this crate carries, shared
//! by the differential tests and the benchmark. Everything is built from a
//! fixed seed so a run is repeatable and a failure names its input.
//!
//! The LSP set is what rust-analyzer sends an editor: a completion list
//! with markdown docs, a semantic token array, diagnostics, a hover and a
//! didChange. Sway's tree is the other live source. The classic three are
//! shaped like the nativejson-benchmark files of the same names.

#![allow(dead_code)]

use std::fmt::Write as _;
use std::string::String;
use std::vec;
use std::vec::Vec;

pub struct Corpus {
    pub name: &'static str,
    pub text: String,
}

/// splitmix64.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    pub fn pick<'a>(&mut self, of: &[&'a str]) -> &'a str {
        of[self.below(of.len())]
    }
}

/// Every corpus at its benchmark size.
pub fn all() -> Vec<Corpus> {
    vec![
        Corpus {
            name: "lsp_completion",
            text: completion(20_000),
        },
        Corpus {
            name: "lsp_semantic_tokens",
            text: semantic_tokens(400_000),
        },
        Corpus {
            name: "lsp_diagnostics",
            text: diagnostics(3_000),
        },
        Corpus {
            name: "lsp_hover",
            text: hover(600),
        },
        Corpus {
            name: "lsp_did_change",
            text: did_change(0),
        },
        Corpus {
            name: "sway_tree",
            text: sway_tree(3, 6, 5),
        },
        Corpus {
            name: "twitter",
            text: twitter(400),
        },
        Corpus {
            name: "canada",
            text: canada(60_000),
        },
        Corpus {
            name: "citm",
            text: citm(180),
        },
    ]
}

/// The same set at a size the differential tests can afford.
pub fn small() -> Vec<Corpus> {
    vec![
        Corpus {
            name: "lsp_completion",
            text: completion(40),
        },
        Corpus {
            name: "lsp_semantic_tokens",
            text: semantic_tokens(300),
        },
        Corpus {
            name: "lsp_diagnostics",
            text: diagnostics(20),
        },
        Corpus {
            name: "lsp_hover",
            text: hover(12),
        },
        Corpus {
            name: "lsp_did_change",
            text: did_change(7),
        },
        Corpus {
            name: "sway_tree",
            text: sway_tree(1, 2, 2),
        },
        Corpus {
            name: "twitter",
            text: twitter(4),
        },
        Corpus {
            name: "canada",
            text: canada(200),
        },
        Corpus {
            name: "citm",
            text: citm(3),
        },
    ]
}

const WORDS: &[&str] = &[
    "frame", "render", "buffer", "layout", "node", "span", "arena", "index", "stack", "token",
    "surface", "output", "region", "cursor", "glyph", "atlas", "scene", "widget", "focus", "input",
];

/// A line of prose with nothing to escape.
pub const WORDS_TEXT: &str = "frame render buffer layout node span arena index stack token surface output region cursor glyph atlas scene widget focus input. ";

/// A paragraph of markdown as a JSON string body, escapes and all.
fn markdown(rng: &mut Rng, s: &mut String, paras: usize) {
    for p in 0..paras {
        if p > 0 {
            s.push_str("\\n\\n");
        }
        match rng.below(5) {
            0 => {
                s.push_str("```rust\\nlet x = \\\"quoted\\\\tvalue\\\";\\nlet y = Some(");
                s.push_str(rng.pick(WORDS));
                s.push_str(");\\n```");
            }
            1 => {
                s.push_str("# ");
                s.push_str(rng.pick(WORDS));
                s.push_str("\\n\\n- `");
                s.push_str(rng.pick(WORDS));
                s.push_str("` \\u2192 the ");
                s.push_str(rng.pick(WORDS));
                s.push_str("\\n- *see also* [");
                s.push_str(rng.pick(WORDS));
                s.push_str("](https:\\/\\/docs.rs\\/x)");
            }
            _ => {
                for w in 0..12 + rng.below(20) {
                    if w > 0 {
                        s.push(' ');
                    }
                    s.push_str(rng.pick(WORDS));
                }
                s.push_str(if rng.below(4) == 0 { " é ✓." } else { "." });
            }
        }
    }
}

fn range(s: &mut String, line: usize, from: usize, to: usize) {
    let _ = write!(
        s,
        r#"{{"start":{{"line":{line},"character":{from}}},"end":{{"line":{line},"character":{to}}}}}"#
    );
}

/// A rust-analyzer completion response with `n` items.
pub fn completion(n: usize) -> String {
    let mut rng = Rng(1);
    let mut s = String::from(r#"{"jsonrpc":"2.0","id":42,"result":{"isIncomplete":true,"items":["#);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        let w = rng.pick(WORDS);
        let _ = write!(
            s,
            r#"{{"label":"{w}_{i}","kind":{},"detail":"fn(&mut self, {w}: &{}) -> Result<(), Error>","documentation":{{"kind":"markdown","value":""#,
            1 + rng.below(25),
            rng.pick(WORDS)
        );
        let paras = 1 + rng.below(4);
        markdown(&mut rng, &mut s, paras);
        let _ = write!(
            s,
            r#""}},"deprecated":false,"preselect":{},"sortText":"ffff{i:08}","filterText":"{w}_{i}","insertTextFormat":2,"textEdit":{{"range":"#,
            rng.below(9) == 0
        );
        range(&mut s, i, 4, 7);
        let _ = write!(
            s,
            r#","newText":"{w}_{i}(${{1}})"}},"additionalTextEdits":[],"data":{{"position":{{"textDocument":{{"uri":"file:///home/u/p/src/main.rs"}},"position":{{"line":10,"character":7}}}},"imports":[],"version":3}}}}"#
        );
    }
    s.push_str("]}}");
    s
}

/// A semantic tokens response with `n` five-int tokens.
pub fn semantic_tokens(n: usize) -> String {
    let mut s = String::from(r#"{"jsonrpc":"2.0","id":7,"result":{"resultId":"1","data":["#);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            "{},{},{},{},{}",
            i % 3,
            (i * 7) % 40,
            1 + i % 12,
            i % 20,
            i % 4
        );
    }
    s.push_str("]}}");
    s
}

/// publishDiagnostics with `n` diagnostics.
pub fn diagnostics(n: usize) -> String {
    let mut rng = Rng(2);
    let mut s = String::from(
        r#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///home/u/p/src/main.rs","version":12,"diagnostics":["#,
    );
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push_str(r#"{"range":"#);
        range(&mut s, i * 3, 4, 4 + rng.below(30));
        let _ = write!(
            s,
            r#","severity":{},"code":"E0{:03}","codeDescription":{{"href":"https://doc.rust-lang.org/error_codes/E0308.html"}},"source":"rustc","message":"mismatched types\nexpected `{}`, found `&str`\n\n  = note: expected struct `{}<u32>`","relatedInformation":[{{"location":{{"uri":"file:///home/u/p/src/lib.rs","range":"#,
            1 + rng.below(4),
            rng.below(600),
            rng.pick(WORDS),
            rng.pick(WORDS)
        );
        range(&mut s, i * 2, 0, 9);
        s.push_str(r#"},"message":"the type is defined here"}],"tags":[]}"#);
    }
    s.push_str("]}}");
    s
}

/// A hover with `paras` paragraphs of markdown.
pub fn hover(paras: usize) -> String {
    let mut rng = Rng(3);
    let mut s = String::from(
        r#"{"jsonrpc":"2.0","id":9,"result":{"contents":{"kind":"markdown","value":""#,
    );
    markdown(&mut rng, &mut s, paras);
    s.push_str(r#""},"range":"#);
    range(&mut s, 33, 8, 19);
    s.push_str("}}");
    s
}

/// A didChange for one typed character, `k` telling the messages apart.
pub fn did_change(k: usize) -> String {
    let mut s = String::from(
        r#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///home/u/p/src/main.rs","version":"#,
    );
    let _ = write!(s, "{}", 40 + k);
    s.push_str(r#"},"contentChanges":[{"range":"#);
    range(&mut s, 10 + k, 4, 4);
    s.push_str(r#","rangeLength":0,"text":"x"}]}}"#);
    s
}

fn rect(s: &mut String, x: i64, y: i64, w: i64, h: i64) {
    let _ = write!(s, r#"{{"x":{x},"y":{y},"width":{w},"height":{h}}}"#);
}

fn sway_node(rng: &mut Rng, s: &mut String, id: &mut u64, kind: &str, depth: usize, width: usize) {
    *id += 1;
    let _ = write!(
        s,
        r#"{{"id":{},"type":"{kind}","orientation":"{}","percent":{}.{},"urgent":false,"marks":[],"focused":{},"layout":"{}","border":"pixel","current_border_width":2,"rect":"#,
        *id,
        if depth.is_multiple_of(2) {
            "horizontal"
        } else {
            "vertical"
        },
        rng.below(2),
        rng.below(1000),
        rng.below(20) == 0,
        rng.pick(&["splith", "splitv", "tabbed", "stacked", "none"])
    );
    rect(s, 0, 0, 1920, 1080);
    s.push_str(r#","deco_rect":"#);
    rect(s, 0, 0, 0, 0);
    s.push_str(r#","window_rect":"#);
    rect(s, 2, 2, 1916, 1076);
    s.push_str(r#","geometry":"#);
    rect(s, 0, 0, 1916, 1076);
    let _ = write!(
        s,
        r#","name":"{}","window":null,"nodes":["#,
        match rng.below(3) {
            0 => "steinterm: ~\\/sub\\/Programming\\/stein",
            1 => "Firefox - Mozilla Firefox",
            _ => "emacs@host",
        }
    );
    if depth > 0 {
        for i in 0..width {
            if i > 0 {
                s.push(',');
            }
            sway_node(rng, s, id, "con", depth - 1, width);
        }
    }
    let _ = write!(
        s,
        r#"],"floating_nodes":[],"focus":[{}],"fullscreen_mode":0,"sticky":false,"pid":{},"app_id":"{}","visible":true,"shell":"xdg_shell","inhibit_idle":false,"idle_inhibitors":{{"user":"none","application":"none"}}}}"#,
        *id + 1,
        1000 + rng.below(30000),
        rng.pick(&["steinterm", "firefox", "emacs", null_str()])
    );
}

fn null_str() -> &'static str {
    "org.gnome.Nautilus"
}

/// A get_tree reply: `outputs` outputs, `workspaces` each, `depth` of
/// containers under those.
pub fn sway_tree(outputs: usize, workspaces: usize, depth: usize) -> String {
    let mut rng = Rng(4);
    let mut id = 0;
    let mut s = String::from(
        r#"{"id":1,"type":"root","orientation":"horizontal","percent":null,"urgent":false,"marks":[],"focused":false,"layout":"splith","border":"none","current_border_width":0,"rect":"#,
    );
    rect(&mut s, 0, 0, 5760, 1080);
    s.push_str(r#","name":"root","window":null,"nodes":["#);
    for o in 0..outputs {
        if o > 0 {
            s.push(',');
        }
        let _ = write!(
            &mut s,
            r#"{{"id":{},"type":"output","name":"DP-{o}","active":true,"primary":false,"scale":1.0,"make":"Dell Inc.","model":"U2720Q","serial":"ABC123","rect":"#,
            id + 2
        );
        rect(&mut s, 1920 * o as i64, 0, 1920, 1080);
        s.push_str(r#","nodes":["#);
        for w in 0..workspaces {
            if w > 0 {
                s.push(',');
            }
            sway_node(&mut rng, &mut s, &mut id, "workspace", depth.min(2), 2);
        }
        s.push_str("]}");
    }
    s.push_str(r#"],"floating_nodes":[],"focus":[2],"fullscreen_mode":0,"sticky":false}"#);
    s
}

const TWEETS: &[&str] = &[
    "@aym0566x \\n\\n名前:前田あゆみ\\n第一印象:なんか怖っ！\\n今の印象:とりあえずキモい。噛み合わない\\n好きなところ:ぶすでキモいとこ😋✨✨",
    "RT @KATANA77: えっそれは・・・(一同) http:\\/\\/t.co\\/PkCJAcSuYK",
    "one more plain english tweet about the layout engine and its arena",
    "\\\"quoted\\\" tweet with a tab\\t and a backslash \\\\ in it",
];

fn twitter_user(rng: &mut Rng, s: &mut String, id: usize) {
    let _ = write!(
        s,
        "      \"user\": {{\n        \"id\": {id},\n        \"id_str\": \"{id}\",\n        \"name\": \"{}\",\n        \"screen_name\": \"{}_{id}\",\n        \"location\": \"\",\n        \"description\": \"{}\",\n        \"url\": null,\n        \"entities\": {{\n          \"description\": {{\n            \"urls\": []\n          }}\n        }},\n        \"protected\": false,\n        \"followers_count\": {},\n        \"friends_count\": {},\n        \"listed_count\": 0,\n        \"created_at\": \"Sat Aug 09 21:09:08 +0000 2014\",\n        \"favourites_count\": {},\n        \"utc_offset\": null,\n        \"time_zone\": null,\n        \"geo_enabled\": false,\n        \"verified\": false,\n        \"statuses_count\": {},\n        \"lang\": \"ja\",\n        \"profile_background_color\": \"C0DEED\",\n        \"profile_image_url\": \"http:\\/\\/pbs.twimg.com\\/profile_images\\/{id}\\/normal.jpeg\",\n        \"default_profile\": true,\n        \"following\": false,\n        \"notifications\": false\n      }},\n",
        rng.pick(&["前田あゆみ", "Ayumi", "山田太郎"]),
        rng.pick(WORDS),
        rng.pick(TWEETS),
        rng.below(5000),
        rng.below(900),
        rng.below(30000),
        rng.below(70000)
    );
}

/// A search response with `n` statuses, pretty printed the way the
/// classic file is.
pub fn twitter(n: usize) -> String {
    let mut rng = Rng(5);
    let mut s = String::from("{\n  \"statuses\": [\n");
    for i in 0..n {
        let id = 505_874_924_095_815_700u64 + i as u64;
        let _ = write!(
            s,
            "    {{\n      \"metadata\": {{\n        \"result_type\": \"recent\",\n        \"iso_language_code\": \"ja\"\n      }},\n      \"created_at\": \"Sun Aug 31 00:29:15 +0000 2014\",\n      \"id\": {id},\n      \"id_str\": \"{id}\",\n      \"text\": \"{}\",\n      \"source\": \"<a href=\\\"http:\\/\\/twitter.com\\/download\\/iphone\\\" rel=\\\"nofollow\\\">Twitter for iPhone<\\/a>\",\n      \"truncated\": false,\n      \"in_reply_to_status_id\": null,\n      \"in_reply_to_user_id_str\": null,\n",
            rng.pick(TWEETS)
        );
        twitter_user(&mut rng, &mut s, 1_186_275_104 + i);
        let _ = write!(
            s,
            "      \"geo\": null,\n      \"coordinates\": null,\n      \"retweet_count\": {},\n      \"favorite_count\": {},\n      \"entities\": {{\n        \"hashtags\": [],\n        \"symbols\": [],\n        \"urls\": [],\n        \"user_mentions\": [\n          {{\n            \"screen_name\": \"aym0566x\",\n            \"name\": \"前田あゆみ\",\n            \"id\": 866260188,\n            \"id_str\": \"866260188\",\n            \"indices\": [\n              0,\n              9\n            ]\n          }}\n        ]\n      }},\n      \"favorited\": false,\n      \"retweeted\": false,\n      \"lang\": \"ja\"\n    }}{}\n",
            rng.below(400),
            rng.below(50),
            if i + 1 < n { "," } else { "" }
        );
    }
    s.push_str("  ],\n  \"search_metadata\": {\n    \"completed_in\": 0.087,\n    \"max_id\": 505874924095815681,\n    \"max_id_str\": \"505874924095815681\",\n    \"next_results\": \"?max_id=505874847260352512&q=%E4%B8%80&count=100&include_entities=1\",\n    \"query\": \"%E4%B8%80\",\n    \"refresh_url\": \"?since_id=505874924095815681&q=%E4%B8%80&include_entities=1\",\n    \"count\": 100,\n    \"since_id\": 0,\n    \"since_id_str\": \"0\"\n  }\n}\n");
    s
}

/// A polygon of `n` points with the long decimals of the classic file.
pub fn canada(n: usize) -> String {
    let mut rng = Rng(6);
    let mut s = String::from(
        "{\"type\":\"FeatureCollection\",\"features\":[{\"type\":\"Feature\",\"properties\":{\"name\":\"Canada\"},\"geometry\":{\"type\":\"Polygon\",\"coordinates\":[[",
    );
    let (mut lon, mut lat) = (-65.613_617_f64, 43.420_273_f64);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        lon += (rng.below(2000) as f64 - 1000.0) * 1e-5;
        lat += (rng.below(2000) as f64 - 1000.0) * 1e-5;
        let _ = write!(s, "[{lon:.15},{lat:.15}]");
        if i % 64 == 63 {
            s.push('\n');
        }
    }
    s.push_str("]]}}]}");
    s
}

fn citm_performance(rng: &mut Rng, s: &mut String, id: usize) {
    let _ = write!(
        s,
        "    {{\n      \"eventId\": {},\n      \"id\": {id},\n      \"logo\": null,\n      \"name\": null,\n      \"prices\": [\n",
        138_586_341 + rng.below(20)
    );
    for p in 0..4 {
        let _ = write!(
            s,
            "        {{\n          \"amount\": {},\n          \"audienceSubCategoryId\": 337100890,\n          \"seatCategoryId\": {}\n        }}{}\n",
            1000 + rng.below(20000),
            338_937_290 + p,
            if p < 3 { "," } else { "" }
        );
    }
    s.push_str("      ],\n      \"seatCategories\": [\n");
    for c in 0..4 {
        let _ = write!(
            s,
            "        {{\n          \"areas\": [\n            {{\n              \"areaId\": {},\n              \"blockIds\": []\n            }},\n            {{\n              \"areaId\": {},\n              \"blockIds\": []\n            }}\n          ],\n          \"seatCategoryId\": {}\n        }}{}\n",
            205_705_990 + rng.below(20),
            205_705_990 + rng.below(20),
            338_937_290 + c,
            if c < 3 { "," } else { "" }
        );
    }
    let _ = write!(
        s,
        "      ],\n      \"seatMapImage\": null,\n      \"start\": {},\n      \"venueCode\": \"PLEYEL_PLEYEL\"\n    }}",
        1_372_701_600_000u64 + rng.below(10_000) as u64 * 3_600_000
    );
}

/// An event catalog with `n` events and their performances.
pub fn citm(n: usize) -> String {
    let mut rng = Rng(7);
    let mut s = String::from("{\n  \"areaNames\": {\n");
    let areas = [
        "Arrière-scène central",
        "1er balcon central",
        "2ème balcon bergerie cour",
        "Orchestre central",
    ];
    for (i, a) in areas.iter().enumerate() {
        let _ = writeln!(
            s,
            "    \"{}\": \"{a}\"{}",
            205_705_993 + i,
            if i + 1 < areas.len() { "," } else { "" }
        );
    }
    s.push_str("  },\n  \"audienceSubCategoryNames\": {\n    \"337100890\": \"Abonné\"\n  },\n  \"blockNames\": {},\n  \"events\": {\n");
    for i in 0..n {
        let _ = write!(
            s,
            "    \"{}\": {{\n      \"description\": null,\n      \"id\": {},\n      \"logo\": \"/images/UE0AAAAACEKo6QAAAAZDSVRN\",\n      \"name\": \"{} {}\",\n      \"subTopicIds\": [\n        337184269,\n        337184283\n      ],\n      \"subjectCode\": null,\n      \"subtitle\": null,\n      \"topicIds\": [\n        324846099,\n        107888604\n      ]\n    }}{}\n",
            138_586_341 + i,
            138_586_341 + i,
            rng.pick(&[
                "30th Anniversary Tour",
                "Berliner Philharmoniker",
                "Les Siècles"
            ]),
            rng.pick(WORDS),
            if i + 1 < n { "," } else { "" }
        );
    }
    s.push_str("  },\n  \"performances\": [\n");
    for i in 0..n * 3 {
        citm_performance(&mut rng, &mut s, 339_887_544 + i);
        s.push_str(if i + 1 < n * 3 { ",\n" } else { "\n" });
    }
    s.push_str("  ],\n  \"seatCategoryNames\": {\n    \"338937290\": \"1ère catégorie\",\n    \"338937291\": \"2ème catégorie\"\n  },\n  \"topicNames\": {\n    \"324846099\": \"Musique classique\",\n    \"107888604\": \"Jazz\"\n  },\n  \"venueNames\": {\n    \"PLEYEL_PLEYEL\": \"Salle Pleyel\"\n  }\n}\n");
    s
}
