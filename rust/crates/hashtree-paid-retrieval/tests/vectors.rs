//! Shared wire vectors, generated here from the Rust codec.
//!
//! `test-vectors/paid-retrieval-vectors.json` is checked in and verified by
//! this test and by the TypeScript package `@hashtree/paid-retrieval`, so the
//! two implementations cannot drift. After an intended wire change:
//!
//! `PAID_RETRIEVAL_UPDATE_VECTORS=1 cargo test -p hashtree-paid-retrieval --test vectors`

use std::collections::BTreeMap;
use std::path::PathBuf;

use hashtree_core::BLOB_MAX_BYTES;
use hashtree_paid_retrieval::bencode::{self, Value};
use hashtree_paid_retrieval::{
    BytePrice, ContentHash, ErrorCode, Quote, Reply, Request, Tariff, BEP10_EXTENSION,
    MAX_HEADER_BYTES, MAX_MESSAGE_BYTES, METHOD_CASHU, PROTOCOL,
};
use serde_json::{json, Value as Json};

const MINT: &str = "https://mint.test";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hash_json(hash: &ContentHash) -> Json {
    let algorithm = match hash {
        ContentHash::Sha256(_) => "sha256",
        ContentHash::Sha1(_) => "sha1",
    };
    json!({ "algorithm": algorithm, "digest": hex(hash.digest()) })
}

fn request_json(request: &Request) -> Json {
    match request {
        Request::Get {
            hash,
            account,
            max_msat,
        } => json!({
            "type": "get",
            "hash": hash_json(hash),
            "account": hex(account),
            "max_msat": max_msat.to_string(),
        }),
        Request::Pay {
            account,
            payment,
            method,
            mint,
            token,
        } => json!({
            "type": "pay",
            "account": hex(account),
            "payment": hex(payment),
            "method": method,
            "mint": mint,
            "token": token,
        }),
    }
}

fn reply_json(reply: &Reply) -> Json {
    match reply {
        Reply::Data {
            hash,
            charged_msat,
            balance_msat,
            duplicate,
            data,
        } => json!({
            "type": "data",
            "hash": hash_json(hash),
            "charged_msat": charged_msat.to_string(),
            "balance_msat": balance_msat.to_string(),
            "duplicate": duplicate,
            "data": hex(data),
        }),
        Reply::Quote(quote) => json!({
            "type": "quote",
            "hash": hash_json(&quote.hash),
            "size": quote.size.to_string(),
            "price_msat": quote.price_msat.to_string(),
            "balance_msat": quote.balance_msat.to_string(),
            "grace_msat": quote.grace_msat.to_string(),
            "tariff": {
                "msat": quote.tariff.rate.msat.to_string(),
                "per_bytes": quote.tariff.rate.per_bytes.to_string(),
                "min_msat": quote.tariff.min_msat.to_string(),
            },
            "mints": quote.mints,
            "methods": quote.methods,
            "min_topup_sat": quote.min_topup_sat.to_string(),
        }),
        Reply::NoResult { hash } => json!({ "type": "none", "hash": hash_json(hash) }),
        Reply::Paid {
            payment,
            credited_msat,
            balance_msat,
        } => json!({
            "type": "paid",
            "payment": hex(payment),
            "credited_msat": credited_msat.to_string(),
            "balance_msat": balance_msat.to_string(),
        }),
        Reply::Error {
            code,
            payment,
            detail,
        } => json!({
            "type": "err",
            "code": code.as_str(),
            "payment": payment.as_ref().map(|payment| hex(payment)),
            "detail": detail,
        }),
    }
}

/// A piece of a wire message: literal bytes, or one byte repeated.
enum Part {
    Bytes(Vec<u8>),
    Repeat(u8, usize),
}

fn lit(bytes: impl AsRef<[u8]>) -> Part {
    Part::Bytes(bytes.as_ref().to_vec())
}

fn build(parts: &[Part]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        match part {
            Part::Bytes(bytes) => out.extend_from_slice(bytes),
            Part::Repeat(byte, times) => out.resize(out.len() + times, *byte),
        }
    }
    out
}

fn parts_json(parts: &[Part]) -> Json {
    parts
        .iter()
        .map(|part| match part {
            Part::Bytes(bytes) => json!(hex(bytes)),
            Part::Repeat(byte, times) => json!({ "repeat": hex(&[*byte]), "times": times }),
        })
        .collect()
}

fn text(value: &str) -> Value {
    Value::Bytes(value.as_bytes().to_vec())
}

fn raw(value: &[u8]) -> Value {
    Value::Bytes(value.to_vec())
}

fn texts(values: &[&str]) -> Value {
    Value::List(values.iter().map(|value| text(value)).collect())
}

/// Canonical bencode of a dictionary with arbitrary fields.
fn dict(entries: &[(&str, Value)]) -> Vec<u8> {
    let entries: BTreeMap<Vec<u8>, Value> = entries
        .iter()
        .map(|(key, value)| (key.as_bytes().to_vec(), value.clone()))
        .collect();
    let mut out = Vec::new();
    bencode::encode(&Value::Dict(entries), &mut out);
    out
}

/// `fields` with one of them replaced or added.
fn with<'a>(fields: &[(&'a str, Value)], key: &'a str, value: Value) -> Vec<(&'a str, Value)> {
    let mut fields: Vec<_> = fields
        .iter()
        .filter(|(name, _)| *name != key)
        .cloned()
        .collect();
    fields.push((key, value));
    fields
}

fn without<'a>(fields: &[(&'a str, Value)], key: &str) -> Vec<(&'a str, Value)> {
    fields
        .iter()
        .filter(|(name, _)| *name != key)
        .cloned()
        .collect()
}

#[derive(Default)]
struct Vectors {
    messages: Vec<Json>,
    acceptance: Vec<Json>,
}

impl Vectors {
    fn message(&mut self, name: &str, kind: &str, direction: &str, wire: &[u8], message: Json) {
        self.messages.push(json!({
            "name": name,
            "kind": kind,
            "direction": direction,
            "wire": hex(wire),
            "message": message,
        }));
    }

    /// Encodes to `wire` and decodes back to the same message.
    fn request(&mut self, name: &str, request: Request) {
        let wire = request.encode();
        assert_eq!(Request::decode(&wire).as_ref(), Ok(&request), "{name}");
        self.message(name, "request", "both", &wire, request_json(&request));
    }

    /// Encodes to `wire`, which decodes to something else.
    fn request_encodes(&mut self, name: &str, request: Request) {
        let wire = request.encode();
        assert_ne!(Request::decode(&wire).as_ref(), Ok(&request), "{name}");
        self.message(name, "request", "encode", &wire, request_json(&request));
    }

    /// Accepted, but not what the encoder writes for the decoded message.
    fn request_decodes(&mut self, name: &str, wire: &[u8]) {
        let request = Request::decode(wire).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_ne!(request.encode(), wire, "{name}");
        self.message(name, "request", "decode", wire, request_json(&request));
    }

    fn reply(&mut self, name: &str, reply: Reply) {
        let wire = reply.encode();
        assert_eq!(Reply::decode(&wire).as_ref(), Ok(&reply), "{name}");
        self.message(name, "reply", "both", &wire, reply_json(&reply));
    }

    fn reply_encodes(&mut self, name: &str, reply: Reply) {
        let wire = reply.encode();
        assert_ne!(Reply::decode(&wire).as_ref(), Ok(&reply), "{name}");
        self.message(name, "reply", "encode", &wire, reply_json(&reply));
    }

    fn reply_decodes(&mut self, name: &str, wire: &[u8]) {
        let reply = Reply::decode(wire).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_ne!(reply.encode(), wire, "{name}");
        self.message(name, "reply", "decode", wire, reply_json(&reply));
    }

    /// Whether a decoder takes `parts`. `kind` is `request`, `reply` or
    /// `bencode` (one value from the start of the input).
    fn accept(&mut self, name: &str, kind: &str, expected: bool, parts: &[Part]) {
        let wire = build(parts);
        let (accepted, used) = match kind {
            "request" => (Request::decode(&wire).is_ok(), None),
            "reply" => (Reply::decode(&wire).is_ok(), None),
            "bencode" => match bencode::decode_prefix(&wire) {
                Ok((_, used)) => (true, Some(used)),
                Err(_) => (false, None),
            },
            _ => panic!("unknown kind {kind}"),
        };
        assert_eq!(accepted, expected, "{name}");
        let mut entry = json!({
            "name": name,
            "kind": kind,
            "accepted": accepted,
            "wire": parts_json(parts),
        });
        if let Some(used) = used {
            entry["used"] = json!(used);
        }
        self.acceptance.push(entry);
    }

    fn accept_bytes(&mut self, name: &str, kind: &str, expected: bool, wire: &[u8]) {
        self.accept(name, kind, expected, &[lit(wire)]);
    }
}

fn account() -> [u8; 32] {
    std::array::from_fn(|index| index as u8)
}

fn payment() -> [u8; 16] {
    std::array::from_fn(|index| 0xf0 + index as u8)
}

fn sha256_name() -> ContentHash {
    ContentHash::Sha256(std::array::from_fn(|index| 0x80 + index as u8))
}

fn sha1_name() -> ContentHash {
    ContentHash::Sha1(std::array::from_fn(|index| 0x40 + index as u8))
}

fn quote() -> Quote {
    Quote {
        hash: sha256_name(),
        size: 2_097_152,
        price_msat: 2_048,
        balance_msat: 100,
        grace_msat: 0,
        tariff: Tariff {
            rate: BytePrice::new(1, 1024),
            min_msat: 1_000,
        },
        mints: vec![MINT.to_owned()],
        methods: vec![METHOD_CASHU.to_owned()],
        min_topup_sat: 1,
    }
}

fn requests(vectors: &mut Vectors) {
    vectors.request(
        "get_sha256",
        Request::Get {
            hash: sha256_name(),
            account: account(),
            max_msat: 16_384,
        },
    );
    vectors.request(
        "get_sha1",
        Request::Get {
            hash: sha1_name(),
            account: account(),
            max_msat: 1,
        },
    );
    vectors.request(
        "get_authorising_nothing",
        Request::Get {
            hash: ContentHash::Sha256([0; 32]),
            account: [0xff; 32],
            max_msat: 0,
        },
    );
    vectors.request(
        "get_largest_number",
        Request::Get {
            hash: sha256_name(),
            account: account(),
            max_msat: i64::MAX as u64,
        },
    );
    vectors.request_encodes(
        "get_above_the_largest_number_saturates",
        Request::Get {
            hash: sha256_name(),
            account: account(),
            max_msat: u64::MAX,
        },
    );
    vectors.request(
        "pay_cashu",
        Request::Pay {
            account: account(),
            payment: payment(),
            method: METHOD_CASHU.to_owned(),
            mint: MINT.to_owned(),
            token: "cashuBexample".to_owned(),
        },
    );
    vectors.request(
        "pay_empty_and_non_ascii_text",
        Request::Pay {
            account: account(),
            payment: [0; 16],
            method: String::new(),
            mint: "https://minttu.test/säästö".to_owned(),
            token: "\u{feff}token ✓".to_owned(),
        },
    );

    let get = [
        ("t", text("get")),
        ("h", raw(sha256_name().digest())),
        ("a", raw(&account())),
        ("x", Value::Int(7)),
    ];
    vectors.request_decodes(
        "get_with_unknown_keys",
        &dict(&with(
            &with(&get, "z", Value::List(vec![Value::Int(-1), text("later")])),
            "A",
            text("ignored"),
        )),
    );
}

fn replies(vectors: &mut Vectors) {
    vectors.reply(
        "data_charged",
        Reply::Data {
            hash: sha256_name(),
            charged_msat: 2_048,
            balance_msat: 2_952,
            duplicate: false,
            data: b"blob bytes".to_vec(),
        },
    );
    vectors.reply(
        "data_free_repeat_on_a_debt",
        Reply::Data {
            hash: sha256_name(),
            charged_msat: 1_000,
            balance_msat: -1_000,
            duplicate: true,
            data: vec![0, 1, 2, 0xff, b'e', b'd'],
        },
    );
    vectors.reply(
        "data_empty",
        Reply::Data {
            hash: sha256_name(),
            charged_msat: 0,
            balance_msat: 0,
            duplicate: false,
            data: Vec::new(),
        },
    );
    vectors.reply(
        "data_sha1_lowest_balance",
        Reply::Data {
            hash: sha1_name(),
            charged_msat: i64::MAX as u64,
            balance_msat: i64::MIN,
            duplicate: false,
            data: b"piece".to_vec(),
        },
    );
    vectors.reply_encodes(
        "data_charge_above_the_largest_number_saturates",
        Reply::Data {
            hash: sha256_name(),
            charged_msat: u64::MAX,
            balance_msat: 0,
            duplicate: false,
            data: b"x".to_vec(),
        },
    );
    vectors.reply("quote", Reply::Quote(quote()));
    vectors.reply(
        "quote_sha1_with_grace_and_a_debt",
        Reply::Quote(Quote {
            hash: sha1_name(),
            size: 262_144,
            price_msat: 256,
            balance_msat: -1_500,
            grace_msat: 1_500,
            min_topup_sat: 21,
            ..quote()
        }),
    );
    vectors.reply(
        "quote_without_mints_or_methods",
        Reply::Quote(Quote {
            mints: Vec::new(),
            methods: Vec::new(),
            tariff: Tariff::FREE,
            ..quote()
        }),
    );
    vectors.reply(
        "quote_largest_numbers_and_sixteen_mints",
        Reply::Quote(Quote {
            hash: sha256_name(),
            size: i64::MAX as u64,
            price_msat: i64::MAX as u64,
            balance_msat: i64::MAX,
            grace_msat: i64::MAX as u64,
            tariff: Tariff {
                rate: BytePrice::new(i64::MAX as u64, i64::MAX as u64),
                min_msat: i64::MAX as u64,
            },
            mints: (0..16)
                .map(|index| format!("https://mint{index}.test/"))
                .collect(),
            methods: vec![METHOD_CASHU.to_owned(), "cashu-spilman".to_owned()],
            min_topup_sat: i64::MAX as u64,
        }),
    );
    vectors.reply_encodes(
        "quote_above_the_largest_number_saturates",
        Reply::Quote(Quote {
            size: u64::MAX,
            price_msat: u64::MAX,
            grace_msat: u64::MAX,
            tariff: Tariff {
                rate: BytePrice::new(u64::MAX, u64::MAX),
                min_msat: u64::MAX,
            },
            min_topup_sat: u64::MAX,
            ..quote()
        }),
    );
    vectors.reply(
        "none",
        Reply::NoResult {
            hash: sha256_name(),
        },
    );
    vectors.reply("none_sha1", Reply::NoResult { hash: sha1_name() });
    vectors.reply(
        "paid",
        Reply::Paid {
            payment: payment(),
            credited_msat: 5_000,
            balance_msat: 5_000,
        },
    );
    vectors.reply(
        "paid_still_in_debt",
        Reply::Paid {
            payment: payment(),
            credited_msat: 1_000,
            balance_msat: -500,
        },
    );
    for code in [
        ErrorCode::Bad,
        ErrorCode::Mint,
        ErrorCode::Method,
        ErrorCode::Invalid,
        ErrorCode::Replay,
        ErrorCode::Retry,
        ErrorCode::Full,
        ErrorCode::Unknown,
    ] {
        vectors.reply(
            &format!("err_{}", code.as_str()),
            Reply::Error {
                code,
                payment: (code != ErrorCode::Bad).then(payment),
                detail: format!("why: {}", code.as_str()),
            },
        );
    }
    vectors.reply(
        "err_empty_and_non_ascii_detail",
        Reply::Error {
            code: ErrorCode::Retry,
            payment: None,
            detail: "määrä ✓ 🙂".to_owned(),
        },
    );
    vectors.reply(
        "err_detail_of_64_characters",
        Reply::Error {
            code: ErrorCode::Retry,
            payment: None,
            detail: "🙂".repeat(64),
        },
    );
    vectors.reply_encodes(
        "err_detail_is_cut_to_64_characters",
        Reply::Error {
            code: ErrorCode::Retry,
            payment: None,
            detail: format!("{}ä", "🙂".repeat(64)),
        },
    );

    let data = [
        ("t", text("data")),
        ("h", raw(sha256_name().digest())),
        ("n", Value::Int(3)),
        ("c", Value::Int(5)),
        ("b", Value::Int(-5)),
    ];
    let with_payload = |fields: &[(&str, Value)]| [dict(fields), b"abc".to_vec()].concat();
    vectors.reply_decodes(
        "data_repeat_flag_zero_is_not_a_repeat",
        &with_payload(&with(&data, "d", Value::Int(0))),
    );
    vectors.reply_decodes(
        "data_repeat_flag_other_than_one",
        &with_payload(&with(&data, "d", Value::Int(7))),
    );
    vectors.reply_decodes(
        "data_with_unknown_keys",
        &with_payload(&with(&data, "z", Value::Dict(BTreeMap::new()))),
    );
    vectors.reply_decodes(
        "err_with_a_code_from_the_future",
        &dict(&[("t", text("err")), ("r", text("later")), ("d", text("x"))]),
    );
}

fn bencode_acceptance(vectors: &mut Vectors) {
    let cases: &[(&str, bool, &[u8])] = &[
        ("int_zero", true, b"i0e"),
        ("int_negative", true, b"i-1e"),
        ("int_largest", true, b"i9223372036854775807e"),
        ("int_smallest", true, b"i-9223372036854775808e"),
        ("int_above_largest", false, b"i9223372036854775808e"),
        ("int_below_smallest", false, b"i-9223372036854775809e"),
        ("int_twenty_digits", false, b"i99999999999999999999e"),
        ("int_twenty_one_digits", false, b"i100000000000000000000e"),
        ("int_leading_zero", false, b"i01e"),
        ("int_negative_zero", false, b"i-0e"),
        ("int_negative_leading_zero", false, b"i-01e"),
        ("int_empty", false, b"ie"),
        ("int_only_minus", false, b"i-e"),
        ("int_plus_sign", false, b"i+1e"),
        ("int_space", false, b"i 1e"),
        ("int_fraction", false, b"i1.0e"),
        ("int_hex", false, b"i0x1e"),
        ("int_unterminated", false, b"i1"),
        ("string_empty", true, b"0:"),
        ("string_binary", true, b"3:\x00\xff\x80"),
        ("string_leading_zero_length", false, b"01:a"),
        ("string_double_zero_length", false, b"00:"),
        ("string_negative_length", false, b"-1:a"),
        ("string_longer_than_input", false, b"5:abc"),
        (
            "string_length_out_of_range",
            false,
            b"99999999999999999999:",
        ),
        ("string_length_without_colon", false, b"3abc"),
        ("list_empty", true, b"le"),
        ("list_mixed", true, b"li1e1:ad1:ai2eee"),
        ("list_unterminated", false, b"li1e"),
        ("nesting_five_levels", true, b"llllleeeee"),
        ("nesting_value_at_fifth_level", true, b"lllli1eeeee"),
        ("nesting_six_levels", false, b"lllllleeeeee"),
        ("nesting_value_at_sixth_level", false, b"llllli1eeeeee"),
        ("dict_empty", true, b"de"),
        ("dict_sorted_by_bytes", true, b"d1:Ai1e1:ai2e2:aai3e1:bi4ee"),
        (
            "dict_high_bytes_sort_last",
            true,
            b"d1:\x7fi1e1:\xc3i2e1:\xffi3ee",
        ),
        (
            "dict_high_bytes_out_of_order",
            false,
            b"d1:\xffi1e1:\x7fi2ee",
        ),
        ("dict_unsorted", false, b"d1:bi1e1:ai2ee"),
        ("dict_prefix_after_longer_key", false, b"d2:aai1e1:ai2ee"),
        ("dict_duplicate_key", false, b"d1:ai1e1:ai2ee"),
        ("dict_integer_key", false, b"di1ei2ee"),
        ("dict_key_without_value", false, b"d1:ae"),
        ("dict_unterminated", false, b"d1:ai1e"),
        ("trailing_bytes_are_left_alone", true, b"i1eXYZ"),
        ("unknown_type", false, b"x"),
        ("empty_input", false, b""),
    ];
    for (name, expected, wire) in cases {
        vectors.accept_bytes(&format!("bencode_{name}"), "bencode", *expected, wire);
    }
}

fn request_acceptance(vectors: &mut Vectors) {
    let sha256 = sha256_name();
    let sha1 = sha1_name();
    let get = [
        ("t", text("get")),
        ("h", raw(sha256.digest())),
        ("a", raw(&account())),
        ("x", Value::Int(7)),
    ];
    let pay = [
        ("t", text("pay")),
        ("a", raw(&account())),
        ("i", raw(&payment())),
        ("k", text(METHOD_CASHU)),
        ("m", text(MINT)),
        ("v", text("cashuBexample")),
    ];
    let mut case = |name: &str, expected: bool, fields: &[(&str, Value)]| {
        vectors.accept_bytes(
            &format!("request_{name}"),
            "request",
            expected,
            &dict(fields),
        );
    };
    case("get", true, &get);
    case("get_without_type", false, &without(&get, "t"));
    case("get_without_hash", false, &without(&get, "h"));
    case("get_without_account", false, &without(&get, "a"));
    case("get_without_limit", false, &without(&get, "x"));
    case("unknown_type", false, &with(&get, "t", text("nope")));
    case("type_is_a_number", false, &with(&get, "t", Value::Int(1)));
    case("type_is_not_utf8", false, &with(&get, "t", raw(b"get\xff")));
    case(
        "type_is_case_sensitive",
        false,
        &with(&get, "t", text("Get")),
    );
    case(
        "get_negative_limit",
        false,
        &with(&get, "x", Value::Int(-1)),
    );
    case("get_limit_is_text", false, &with(&get, "x", text("7")));
    case("get_short_account", false, &with(&get, "a", raw(&[1; 31])));
    case("get_long_account", false, &with(&get, "a", raw(&[1; 33])));
    case(
        "get_account_is_a_number",
        false,
        &with(&get, "a", Value::Int(1)),
    );
    case(
        "get_sha256_named_explicitly",
        false,
        &with(&get, "y", text("sha256")),
    );
    case(
        "get_sha1",
        true,
        &with(&with(&get, "h", raw(sha1.digest())), "y", text("sha1")),
    );
    case(
        "get_sha1_digest_without_algorithm",
        false,
        &with(&get, "h", raw(sha1.digest())),
    );
    case(
        "get_sha1_with_sha256_digest",
        false,
        &with(&get, "y", text("sha1")),
    );
    case(
        "get_unknown_algorithm",
        false,
        &with(&get, "y", text("md5")),
    );
    case(
        "get_algorithm_is_a_number",
        false,
        &with(&get, "y", Value::Int(1)),
    );
    case("pay", true, &pay);
    for key in ["a", "i", "k", "m", "v"] {
        case(&format!("pay_without_{key}"), false, &without(&pay, key));
    }
    case("pay_short_id", false, &with(&pay, "i", raw(&[1; 15])));
    case("pay_long_id", false, &with(&pay, "i", raw(&[1; 17])));
    case(
        "pay_method_at_limit",
        true,
        &with(&pay, "k", raw(&[b'k'; 256])),
    );
    case(
        "pay_method_above_limit",
        false,
        &with(&pay, "k", raw(&[b'k'; 257])),
    );
    case(
        "pay_mint_at_limit",
        true,
        &with(&pay, "m", raw(&[b'm'; 512])),
    );
    case(
        "pay_mint_above_limit",
        false,
        &with(&pay, "m", raw(&[b'm'; 513])),
    );
    case(
        "pay_token_is_not_utf8",
        false,
        &with(&pay, "v", raw(b"\xc3\x28")),
    );
    case(
        "pay_token_is_a_list",
        false,
        &with(&pay, "v", texts(&["cashuBexample"])),
    );

    let get_wire = dict(&get);
    vectors.accept(
        "request_trailing_byte",
        "request",
        false,
        &[lit(&get_wire), lit(b"x")],
    );
    vectors.accept_bytes("request_is_a_list", "request", false, b"l3:gete");
    vectors.accept_bytes("request_is_a_number", "request", false, b"i1e");
    vectors.accept_bytes("request_is_empty", "request", false, b"");
    vectors.accept_bytes(
        "request_keys_out_of_order",
        "request",
        false,
        &[
            b"d1:t3:get1:a32:".as_slice(),
            &account(),
            b"1:h32:",
            sha256.digest(),
            b"1:xi7ee",
        ]
        .concat(),
    );

    // A token is at most 48 KiB.
    let pay_wire = dict(&without(&pay, "v"));
    let pay_head = &pay_wire[..pay_wire.len() - 1];
    for (name, expected, length) in [
        ("request_pay_token_at_limit", true, 48 * 1024),
        ("request_pay_token_above_limit", false, 48 * 1024 + 1),
    ] {
        let parts = [
            lit(pay_head),
            lit(format!("1:v{length}:")),
            Part::Repeat(b'A', length),
            lit(b"e"),
        ];
        vectors.accept(name, "request", expected, &parts);
    }

    // The whole dictionary must fit in the 64 KiB header.
    let get_head = &get_wire[..get_wire.len() - 1];
    let prefix = b"1:z65400:";
    let fits = MAX_HEADER_BYTES - get_head.len() - prefix.len() - 1;
    assert_eq!(fits.to_string().len(), 5);
    for (name, expected, length) in [
        ("request_header_at_limit", true, fits),
        ("request_header_above_limit", false, fits + 1),
    ] {
        let parts = [
            lit(get_head),
            lit(format!("1:z{length}:")),
            Part::Repeat(0, length),
            lit(b"e"),
        ];
        vectors.accept(name, "request", expected, &parts);
    }
}

fn reply_acceptance(vectors: &mut Vectors) {
    let sha256 = sha256_name();
    let data = [
        ("t", text("data")),
        ("h", raw(sha256.digest())),
        ("n", Value::Int(3)),
        ("c", Value::Int(5)),
        ("b", Value::Int(-5)),
    ];
    let mut payload_case =
        |name: &str, expected: bool, fields: &[(&str, Value)], payload: &[u8]| {
            vectors.accept(
                &format!("reply_{name}"),
                "reply",
                expected,
                &[lit(dict(fields)), lit(payload)],
            );
        };
    payload_case("data", true, &data, b"abc");
    payload_case("data_shorter_than_declared", false, &data, b"ab");
    payload_case("data_longer_than_declared", false, &data, b"abcd");
    payload_case("data_without_payload", false, &data, b"");
    payload_case("data_without_size", false, &without(&data, "n"), b"abc");
    payload_case("data_without_charge", false, &without(&data, "c"), b"abc");
    payload_case("data_without_balance", false, &without(&data, "b"), b"abc");
    payload_case(
        "data_negative_size",
        false,
        &with(&data, "n", Value::Int(-3)),
        b"abc",
    );
    payload_case(
        "data_negative_charge",
        false,
        &with(&data, "c", Value::Int(-1)),
        b"abc",
    );
    payload_case(
        "data_balance_is_text",
        false,
        &with(&data, "b", text("0")),
        b"abc",
    );
    payload_case(
        "data_negative_repeat_flag",
        false,
        &with(&data, "d", Value::Int(-1)),
        b"abc",
    );
    payload_case(
        "data_repeat_flag_is_text",
        false,
        &with(&data, "d", text("1")),
        b"abc",
    );
    payload_case(
        "data_short_hash",
        false,
        &with(&data, "h", raw(&[1; 31])),
        b"abc",
    );

    // Only the header is bounded by 64 KiB, and a blob by 16 MiB.
    for (name, expected, length) in [
        ("reply_data_larger_than_the_header_limit", true, 70_000),
        ("reply_data_at_blob_limit", true, BLOB_MAX_BYTES),
        ("reply_data_above_blob_limit", false, BLOB_MAX_BYTES + 1),
    ] {
        let header = dict(&with(&data, "n", Value::Int(length as i64)));
        vectors.accept(
            name,
            "reply",
            expected,
            &[lit(header), Part::Repeat(0xab, length)],
        );
    }

    let quote = [
        ("t", text("quote")),
        ("h", raw(sha256.digest())),
        ("n", Value::Int(4_096)),
        ("c", Value::Int(1_000)),
        ("b", Value::Int(0)),
        ("g", Value::Int(0)),
        ("r", Value::List(vec![Value::Int(1), Value::Int(1024)])),
        ("f", Value::Int(1_000)),
        ("m", texts(&[MINT])),
        ("k", texts(&[METHOD_CASHU])),
        ("u", Value::Int(1)),
    ];
    let paid = [
        ("t", text("paid")),
        ("i", raw(&payment())),
        ("v", Value::Int(5_000)),
        ("b", Value::Int(5_000)),
    ];
    let err = [
        ("t", text("err")),
        ("r", text("retry")),
        ("d", text("mint unreachable")),
    ];
    let none = [("t", text("none")), ("h", raw(sha256.digest()))];
    let many: Vec<String> = (0..17)
        .map(|index| format!("https://m{index}.test"))
        .collect();
    let mints = |count: usize| {
        texts(
            &many
                .iter()
                .take(count)
                .map(String::as_str)
                .collect::<Vec<_>>(),
        )
    };
    let ints =
        |values: &[i64]| Value::List(values.iter().map(|value| Value::Int(*value)).collect());

    let mut case = |name: &str, expected: bool, fields: &[(&str, Value)]| {
        vectors.accept_bytes(&format!("reply_{name}"), "reply", expected, &dict(fields));
    };
    case("quote", true, &quote);
    for key in ["h", "n", "c", "b", "g", "r", "f", "m", "k", "u"] {
        case(
            &format!("quote_without_{key}"),
            false,
            &without(&quote, key),
        );
    }
    case(
        "quote_rate_of_one_number",
        false,
        &with(&quote, "r", ints(&[1])),
    );
    case(
        "quote_rate_of_three_numbers",
        false,
        &with(&quote, "r", ints(&[1, 2, 3])),
    );
    case(
        "quote_negative_rate",
        false,
        &with(&quote, "r", ints(&[-1, 1024])),
    );
    case(
        "quote_negative_rate_bytes",
        false,
        &with(&quote, "r", ints(&[1, -1024])),
    );
    case(
        "quote_rate_per_zero_bytes",
        true,
        &with(&quote, "r", ints(&[1, 0])),
    );
    case(
        "quote_rate_is_text",
        false,
        &with(&quote, "r", texts(&["1", "1024"])),
    );
    case(
        "quote_rate_is_a_number",
        false,
        &with(&quote, "r", Value::Int(1)),
    );
    case("quote_sixteen_mints", true, &with(&quote, "m", mints(16)));
    case(
        "quote_seventeen_mints",
        false,
        &with(&quote, "m", mints(17)),
    );
    case(
        "quote_mint_at_limit",
        true,
        &with(&quote, "m", Value::List(vec![raw(&[b'm'; 512])])),
    );
    case(
        "quote_mint_above_limit",
        false,
        &with(&quote, "m", Value::List(vec![raw(&[b'm'; 513])])),
    );
    case(
        "quote_mint_is_not_utf8",
        false,
        &with(&quote, "m", Value::List(vec![raw(b"\xff")])),
    );
    case("quote_mints_is_text", false, &with(&quote, "m", text(MINT)));
    case(
        "quote_method_is_a_number",
        false,
        &with(&quote, "k", ints(&[1])),
    );
    case(
        "quote_negative_price",
        false,
        &with(&quote, "c", Value::Int(-1)),
    );
    case(
        "quote_negative_balance",
        true,
        &with(&quote, "b", Value::Int(-1)),
    );
    case(
        "quote_negative_grace",
        false,
        &with(&quote, "g", Value::Int(-1)),
    );
    case(
        "quote_negative_top_up",
        false,
        &with(&quote, "u", Value::Int(-1)),
    );
    case(
        "quote_unknown_algorithm",
        false,
        &with(&quote, "y", text("blake3")),
    );
    case("none", true, &none);
    case("none_without_hash", false, &without(&none, "h"));
    case("paid", true, &paid);
    for key in ["i", "v", "b"] {
        case(&format!("paid_without_{key}"), false, &without(&paid, key));
    }
    case("paid_short_id", false, &with(&paid, "i", raw(&[1; 15])));
    case(
        "paid_negative_credit",
        false,
        &with(&paid, "v", Value::Int(-1)),
    );
    case("err", true, &err);
    case("err_without_code", false, &without(&err, "r"));
    case("err_without_detail", false, &without(&err, "d"));
    case(
        "err_code_is_a_number",
        false,
        &with(&err, "r", Value::Int(1)),
    );
    case("err_with_id", true, &with(&err, "i", raw(&payment())));
    case("err_with_short_id", false, &with(&err, "i", raw(&[1; 15])));
    case("err_id_is_a_number", false, &with(&err, "i", Value::Int(1)));
    case(
        "err_detail_at_limit",
        true,
        &with(&err, "d", raw(&[b'd'; 256])),
    );
    case(
        "err_detail_above_limit",
        false,
        &with(&err, "d", raw(&[b'd'; 257])),
    );
    case(
        "err_detail_is_not_utf8",
        false,
        &with(&err, "d", raw(b"\xed\xa0\x80")),
    );
    case("unknown_type", false, &with(&none, "t", text("nope")));
    case("request_type", false, &with(&none, "t", text("get")));
    case("without_type", false, &without(&none, "t"));

    let none_wire = dict(&none);
    vectors.accept(
        "reply_none_trailing_byte",
        "reply",
        false,
        &[lit(&none_wire), lit(b"x")],
    );
    vectors.accept_bytes("reply_is_a_number", "reply", false, b"i1e");
    vectors.accept_bytes("reply_is_truncated", "reply", false, b"d1:h32:");
    vectors.accept_bytes("reply_is_empty", "reply", false, b"");
}

fn prices() -> Json {
    let cases: [(u64, u64, u64, u64); 12] = [
        (1, 1024, 5, 0),
        (1, 1024, 5, 1),
        (1, 1024, 5, 6 * 1024),
        (1, 1024, 5, 6 * 1024 + 1),
        (1, 1024, 1_000, 2 * 1024 * 1024),
        (1, 1024, 100, BLOB_MAX_BYTES as u64),
        (0, 1, 0, BLOB_MAX_BYTES as u64),
        (3, 0, 0, 2),
        (7, 3, 0, 10),
        (u64::MAX, 1, 0, u64::MAX),
        (u64::MAX, u64::MAX, 0, u64::MAX),
        (1, u64::MAX, u64::MAX, 1),
    ];
    cases
        .into_iter()
        .map(|(msat, per_bytes, min_msat, bytes)| {
            let tariff = Tariff {
                rate: BytePrice::new(msat, per_bytes),
                min_msat,
            };
            json!({
                "msat": msat.to_string(),
                "per_bytes": per_bytes.to_string(),
                "min_msat": min_msat.to_string(),
                "bytes": bytes.to_string(),
                "price_msat": tariff.price_msat(bytes).to_string(),
            })
        })
        .collect()
}

fn generate() -> Json {
    let mut vectors = Vectors::default();
    requests(&mut vectors);
    replies(&mut vectors);
    bencode_acceptance(&mut vectors);
    request_acceptance(&mut vectors);
    reply_acceptance(&mut vectors);
    json!({
        "description": "Paid retrieval wire vectors. Generated by rust/crates/hashtree-paid-retrieval/tests/vectors.rs; do not edit by hand.",
        "protocol": PROTOCOL,
        "bep10_extension": BEP10_EXTENSION,
        "method_cashu": METHOD_CASHU,
        "max_header_bytes": MAX_HEADER_BYTES,
        "max_message_bytes": MAX_MESSAGE_BYTES,
        "blob_max_bytes": BLOB_MAX_BYTES,
        "messages": vectors.messages,
        "acceptance": vectors.acceptance,
        "prices": prices(),
    })
}

#[test]
fn the_shared_vectors_match_this_implementation() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../test-vectors/paid-retrieval-vectors.json");
    let generated = generate();
    if std::env::var_os("PAID_RETRIEVAL_UPDATE_VECTORS").is_some() {
        let mut text = serde_json::to_string_pretty(&generated).unwrap();
        text.push('\n');
        std::fs::write(&path, text).unwrap();
    }
    let saved = std::fs::read_to_string(&path).expect("the shared vectors file is missing");
    let saved: Json = serde_json::from_str(&saved).expect("the shared vectors file is not JSON");
    assert!(
        saved == generated,
        "test-vectors/paid-retrieval-vectors.json no longer matches the Rust codec; \
         if the wire change is intended, regenerate it (see the top of this file) \
         and update the TypeScript codec"
    );
}
