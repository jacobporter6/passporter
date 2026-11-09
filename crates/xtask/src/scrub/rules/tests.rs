//! Rule tests. Every secret here is synthetic and assembled at runtime, so
//! no secret-shaped literal sits in the source for scanners to flag.

use super::*;

fn redactor() -> Redactor {
    Redactor::new().expect("rules compile")
}

fn redact(text: &str) -> (String, Counts) {
    let mut counts = Counts::new();
    let out = redactor()
        .redact(text, &mut counts, &mut Vec::new())
        .into_owned();
    (out, counts)
}

/// `prefix` plus `len` characters drawn from `alphabet`.
pub(crate) fn fake_from(prefix: &str, len: usize, alphabet: &[u8]) -> String {
    let body: String = (0..len)
        .map(|i| char::from(alphabet[(i * 7 + 3) % alphabet.len()]))
        .collect();
    format!("{prefix}{body}")
}

/// Alphanumeric synthetic secret.
pub(crate) fn fake(prefix: &str, len: usize) -> String {
    fake_from(prefix, len, b"aZ3kQ9mX2pL7vR4tW8yB1nC6")
}

/// A synthetic Google API key shaped like real ones: `AIza` + 35 chars of
/// `[0-9A-Za-z_-]`, here ending in `last`.
fn google_key(last: char) -> String {
    let mut key = fake_from("AIzaSy", 32, b"B_q3x-K9mZ2pLw7v");
    key.push(last);
    assert_eq!(key.len(), 39);
    key
}

fn assert_redacted(text: &str, secret: &str, rule: &str) {
    let (out, counts) = redact(text);
    assert!(!out.contains(secret), "{rule}: leaked in {out}");
    assert!(
        out.contains(&format!("[REDACTED:{rule}]")),
        "{rule}: {out} ({counts:?})"
    );
}

#[test]
fn provider_tokens() {
    let cases = [
        (fake("sk-ant-api03-", 40), "anthropic"),
        (fake("sk-proj-", 40), "openai"),
        (fake("ghp_", 36), "github"),
        (fake("github_pat_", 40), "github"),
        (fake("glpat-", 20), "gitlab"),
        (fake("hf_", 34), "huggingface"),
        (fake("npm_", 36), "npm"),
        (fake("tskey-auth-", 30), "tailscale"),
        (
            fake_from("AGE-SECRET-KEY-1", 58, b"QPZRY9X8GF2TVDW0S3JN54KHCE6MUA7L"),
            "age",
        ),
        (fake("dckr_pat_", 27), "docker"),
        (fake("ya29.", 60), "google-oauth"),
        (format!("{}.{}", fake("SG.", 22), fake("", 43)), "sendgrid"),
        (format!("AKIA{}", "Q2W3E4R5T6Y7U8I9"), "aws-access-key-id"),
        (fake("xoxb-", 30), "slack"),
        (google_key('Q'), "google-api-key"),
        (fake("sk_live_", 24), "stripe"),
        (fake("eyJhIjoi", 60), "cloudflare-tunnel"),
        (
            format!("{}.{}.{}", fake("eyJ", 20), fake("eyJ", 30), fake("", 20)),
            "jwt",
        ),
    ];
    for (secret, rule) in cases {
        let (out, counts) = redact(&format!("token here: {secret} end"));
        assert!(!out.contains(&secret), "{rule}: {out}");
        assert!(out.contains(&format!("[REDACTED:{rule}]")), "{rule}: {out}");
        assert_eq!(counts.values().sum::<u64>(), 1, "{rule}: {counts:?}");
    }
}

/// Regression for the real-corpus misses: Google keys that survived.
#[test]
fn google_keys_in_every_context() {
    for last in ['Q', '-', '_', '7'] {
        let key = google_key(last);
        for text in [
            format!(r#"{{"apiKey":"{key}"}}"#),
            format!("key={key}"),
            format!("https://maps.googleapis.com/maps/api/js?key={key}&callback=init"),
            format!("https://x.googleapis.com/v1?key%3D{key}"),
            format!(r"config: \n{key}\n"),
            format!("GOOGLE_API_KEY={key}"),
            key.clone(),
            format!("{key}extra"),
            format!("\"{key}\""),
        ] {
            assert_redacted(&text, &key, "google-api-key");
        }
    }
}

/// Regression for the real-corpus misses: URL credentials.
#[test]
fn url_credentials_with_at_signs_and_empty_users() {
    let pw = fake_from("", 14, b"aB3$dE5^gH7*jK");
    for (text, secret) in [
        (format!("postgres://app:{pw}@10.0.0.5:5432/db"), pw.clone()),
        (
            format!("mongodb://app:p@ss{pw}@db.lan.example/x"),
            format!("p@ss{pw}"),
        ),
        (format!("redis://:{pw}@127.0.0.1:6379"), pw.clone()),
        (format!(r#"{{\"url\":\"amqp://u:{pw}@mq\"}}"#), pw.clone()),
    ] {
        let (out, _) = redact(&text);
        assert!(!out.contains(&secret), "{out}");
        assert!(out.contains("[REDACTED:url-credentials]@"), "{out}");
    }
    let (out, _) = redact("see https://example.com/a@b and user@host");
    assert!(!out.contains("REDACTED"), "{out}");
}

/// Regression for the last real-corpus miss: `postgresql://` URLs whose
/// password contains `/` or whose user contains `@` (Azure-style).
#[test]
fn postgresql_urls_in_every_shape() {
    let pw = fake_from("", 12, b"Kp7$x%Q!2*(w)Z");
    let slashed = format!("{}/{}+=", &pw[..5], &pw[5..]);
    let cases = [
        (
            format!("postgresql://app:{slashed}@db.lan.example:5432/app"),
            slashed.clone(),
        ),
        (
            format!("postgresql://admin@pgsrv:{pw}@pgsrv.postgres.database.azure.com:5432/db"),
            pw.clone(),
        ),
        (
            format!("postgresql://app:{pw}@db:5432/app?sslmode=require"),
            pw.clone(),
        ),
        (
            format!("postgresql+psycopg2://app:{pw}@10.0.0.7/app"),
            pw.clone(),
        ),
        (
            format!("DATABASE_URL='postgresql://app:{pw}@db/app'"),
            pw.clone(),
        ),
        (
            format!(r#"{{\"dsn\": \"postgresql://app:{pw}@db/app\"}}"#),
            pw.clone(),
        ),
        (
            format!("postgresql://app:{pw}@[fd00::5]:5432/app"),
            pw.clone(),
        ),
        (format!("postgresql://app:{pw}@db"), pw.clone()),
        (
            format!("postgresql://app:p@ss{pw}@db/app"),
            format!("p@ss{pw}"),
        ),
    ];
    for (text, secret) in cases {
        let (out, _) = redact(&text);
        assert!(!out.contains(&secret), "leaked in {out}");
        assert!(out.contains("[REDACTED:url-credentials]@"), "{out}");
    }
    // Host and database stay readable.
    let (out, _) = redact(&format!(
        "postgresql://app:{slashed}@db.lan.example:5432/app"
    ));
    assert_eq!(
        out,
        "postgresql://app:[REDACTED:url-credentials]@db.lan.example:5432/app"
    );
    // A port followed by a scoped path is not a password.
    for text in [
        "https://registry.npmjs.org:443/@types/node",
        "http://localhost:8080/@vite/client",
    ] {
        assert_eq!(redact(text).0, text);
    }
}

/// Regression for the real-corpus misses: JWTs.
#[test]
fn jwts_unsigned_and_glued() {
    let header = fake("eyJ", 20);
    let payload = fake("eyJ", 40);
    let signed = format!("{header}.{payload}.{}", fake("", 43));
    let unsigned = format!("{header}.{payload}.");
    for (text, secret) in [
        (format!(r"token\n{signed}"), signed.clone()),
        (format!("?jwt={unsigned}&x=1"), payload.clone()),
        (format!("Cookie-less {signed}"), signed.clone()),
    ] {
        let (out, _) = redact(&text);
        assert!(!out.contains(&secret), "{out}");
    }
}

#[test]
fn contextual_secrets_keep_their_keys() {
    let secret = fake("", 32);
    let cases = [
        (
            format!("Authorization: Bearer {secret}"),
            "Authorization: Bearer [REDACTED:bearer]".to_owned(),
        ),
        (
            format!("curl -H 'x-api-key: {secret}'"),
            "curl -H 'x-api-key: [REDACTED:auth-header]'".to_owned(),
        ),
        (
            format!("export CLOUDFLARE_API_TOKEN={secret}"),
            "export CLOUDFLARE_API_TOKEN=[REDACTED:cloudflare]".to_owned(),
        ),
        (
            format!("DB_PASSWORD={secret}"),
            "DB_PASSWORD=[REDACTED:env-secret]".to_owned(),
        ),
        (
            format!("MY_SERVICE_KEY=\"{secret}\""),
            "MY_SERVICE_KEY=\"[REDACTED:env-secret]\"".to_owned(),
        ),
        (
            format!("password: {secret}"),
            "password: [REDACTED:password]".to_owned(),
        ),
        (
            format!("aws_secret_access_key = {}", fake("", 40)),
            "aws_secret_access_key = [REDACTED:aws-secret-key]".to_owned(),
        ),
        (
            format!("{{\"session_token\": \"{secret}\"}}"),
            "{\"session_token\": \"[REDACTED:high-entropy]\"}".to_owned(),
        ),
    ];
    for (input, want) in cases {
        assert_eq!(redact(&input).0, want, "{input}");
    }
}

#[test]
fn passwords_flags_and_cookies() {
    let words = "correct horse battery";
    let token = fake("", 30);
    let cases = [
        (
            format!("password: \"{words}\""),
            words.to_owned(),
            "password",
        ),
        (
            format!("password = '{words}'"),
            words.to_owned(),
            "password",
        ),
        (
            format!(r#"{{\"password\": \"{words}\"}}"#),
            words.to_owned(),
            "password",
        ),
        (
            format!("mysql --password {token} -h db"),
            token.clone(),
            "cli-credentials",
        ),
        (
            format!("tool --password={token}"),
            token.clone(),
            "cli-credentials",
        ),
        (
            format!("echo x | gh auth login --with-token {token}"),
            token.clone(),
            "cli-credentials",
        ),
        (
            format!("curl -u admin:{token} https://router.lan.example"),
            token.clone(),
            "cli-credentials",
        ),
        (
            format!("Cookie: session={token}; theme=dark"),
            token.clone(),
            "cookie",
        ),
        (
            format!("set-cookie: sid={token}; HttpOnly"),
            token.clone(),
            "cookie",
        ),
        (
            format!("API_KEY=\"{words}\""),
            words.to_owned(),
            "env-secret",
        ),
    ];
    for (text, secret, rule) in cases {
        assert_redacted(&text, &secret, rule);
    }
}

#[test]
fn private_keys_including_truncated_ones() {
    let body = fake("", 64);
    let pem = format!(
        "key:\n-----BEGIN OPENSSH PRIVATE KEY-----\n{body}\n-----END OPENSSH PRIVATE KEY-----\ndone"
    );
    assert_eq!(redact(&pem).0, "key:\n[REDACTED:private-key]\ndone");
    let cut = format!("-----BEGIN RSA PRIVATE KEY-----\n{body}");
    assert_eq!(redact(&cut).0, "[REDACTED:private-key]");
}

#[test]
fn keeps_paths_ips_and_ordinary_code() {
    for text in [
        "/home/dev/git/passporter/crates/transcripts/src/lib.rs",
        "ssh dev@192.0.2.20 -p 22; curl http://10.0.0.5:8080/health",
        "password: string;",
        "const token = process.env.API_TOKEN;",
        "API_TOKEN=${API_TOKEN}",
        "max_tokens: 4096, input_tokens: 12",
        "the auth flow reads token_count = 1234567890123456",
        "export PATH=$HOME/.cargo/bin:$PATH",
        "git commit -m 'feat(auth): rotate keys'",
        "cargo run --release -- --token-file ./t.txt",
        "disk-usage-report-generator-for-servers",
        "git log --user-name x",
    ] {
        let (out, counts) = redact(text);
        assert_eq!(out, text, "{counts:?}");
    }
}

#[test]
fn never_double_counts() {
    let secret = fake("ghp_", 36);
    let (out, counts) = redact(&format!("GITHUB_TOKEN={secret}"));
    assert_eq!(out, "GITHUB_TOKEN=[REDACTED:github]");
    assert_eq!(counts.values().sum::<u64>(), 1, "{counts:?}");
}

#[test]
fn found_secrets_are_reported() {
    let secret = fake("glpat-", 24);
    let mut found = Vec::new();
    redactor().redact(&format!("x {secret} y"), &mut Counts::new(), &mut found);
    assert_eq!(
        found,
        [Found {
            rule: "gitlab",
            secret,
            quoted: false
        }]
    );
}

#[test]
fn secret_keys_in_every_casing() {
    for key in [
        "password",
        "api_key",
        "apiKey",
        "APIKey",
        "X-Api-Key",
        "DB_PASSWORD",
        "client-secret",
        "token",
        "refresh_token",
        "secretKey",
        "apiToken",
        "sessionToken",
        "bearerToken",
        "oauthToken",
        "auth",
        "pass",
        "pwd",
        "cookie",
        "Set-Cookie",
        "githubToken",
        "dbPassword",
        "awsSecretAccessKey",
        "AWS_SECRET_ACCESS_KEY",
        "authorization",
        "secrets",
        "tokens",
        "passwords",
    ] {
        assert!(is_secret_key(key), "{key}");
    }
    for key in [
        "input_tokens",
        "maxTokens",
        "tokenCount",
        "file_path",
        "signature",
        "author",
        "passwordHint",
        "bypass",
        "key",
        "keyboard",
        "oauth",
        "",
    ] {
        assert!(!is_secret_key(key), "{key}");
    }
}

#[test]
fn entropy_threshold() {
    assert!(entropy("aaaaaaaaaaaaaaaa") < 1.0);
    assert!(high_entropy(&fake("", 32)));
    assert!(!high_entropy("abcdefghijklmnop"));
    // No digit needed when the letters are random enough.
    let letters = fake_from("", 32, b"qWeRtYuIoPaSdFgHjKlZxCvBnM");
    assert!(high_entropy(&letters), "{letters}");
    assert!(!high_entropy("getAuthentication"));
    let (out, _) = redact(&format!("session_token = {letters}"));
    assert!(!out.contains(&letters), "{out}");
}

/// Plain-text shapes from the review: compose/YAML/.env names with `:` or
/// `=`, `.netrc`, MySQL and friends, Azure keys, Discord and Telegram.
#[test]
fn config_and_cli_shapes() {
    let pw = fake("", 14);
    let cases = [
        (format!("DB_PASSWORD: {pw}"), "env-secret"),
        (format!("      - MYSQL_ROOT_PASSWORD={pw}"), "env-secret"),
        (format!("  POSTGRES_PASSWORD: \"{pw}\""), "env-secret"),
        (format!("export MY_PW={pw}"), "env-secret"),
        (format!("REDIS_PASS={pw}"), "env-secret"),
        (format!("db_password: {pw}"), "password"),
        (format!("smtp.pass = '{pw}'"), "password"),
        (
            format!("machine github.com login dev password {pw}"),
            "netrc",
        ),
        (
            format!("mysql -uroot -p{pw} app < dump.sql"),
            "cli-password",
        ),
        (format!("mysqldump -h db -u app -p{pw} app"), "cli-password"),
        (format!("sshpass -p {pw} ssh pi@192.0.2.20"), "cli-password"),
        (format!("sshpass -e -p{pw} scp a b"), "cli-password"),
        (
            format!("docker login -u dev -p {pw} ghcr.io"),
            "cli-password",
        ),
        (
            format!(
                "DefaultEndpointsProtocol=https;AccountName=st;AccountKey={pw}==;EndpointSuffix=core.windows.net"
            ),
            "azure-key",
        ),
        (
            format!(
                "Endpoint=sb://ns.servicebus.windows.net/;SharedAccessKeyName=root;SharedAccessKey={pw}="
            ),
            "azure-key",
        ),
        (
            format!("https://st.blob.core.windows.net/c/b?sv=2022&sig={pw}abcdef"),
            "azure-key",
        ),
    ];
    for (text, rule) in cases {
        assert_redacted(&text, &pw, rule);
    }
    let hook = format!(
        "https://discord.com/api/webhooks/123456789012345678/{}",
        fake("", 68)
    );
    assert_redacted(&format!("curl -X POST {hook}"), &hook, "discord-webhook");
    let bot = format!("{}:{}", "1234567890", fake("AA", 33));
    assert_redacted(
        &format!("https://api.telegram.org/bot{bot}/sendMessage"),
        &bot,
        "telegram",
    );
    // Not secrets.
    for text in [
        "mysql -p app",
        "docker login --password-stdin ghcr.io",
        "MONKEY=banana12",
        "the password field is required",
        "PRIMARY_KEY_COLUMN=id",
        "passport: valid",
    ] {
        let (out, counts) = redact(text);
        assert_eq!(out, text, "{counts:?}");
    }
}
