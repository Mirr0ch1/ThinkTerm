use super::*;
use termwiz::color::ColorAttribute;
use wezterm_term::{Terminal, TerminalSize};

const GREEN: ColorAttribute = ColorAttribute::PaletteIndex(2);

fn line(text: &str) -> Line {
    Line::from_text(text, &CellAttributes::default(), SEQ_ZERO, None)
}

fn text_of(line: &Line) -> String {
    line.visible_cells()
        .map(|cell| cell.str().to_string())
        .collect()
}

fn masked(lines: Vec<Line>) -> Vec<String> {
    let mut lines = lines;
    mask_secrets(&mut lines);
    lines.iter().map(text_of).collect()
}

fn dots(count: usize) -> String {
    "•".repeat(count)
}

#[test]
fn secrets_are_masked_and_the_rest_is_left_alone() {
    let key = format!("sk-{}", "a".repeat(24));
    let texts = masked(vec![
        line(&format!("export EXAMPLE_API_KEY={key}")),
        line("API_TOKEN=abcdefgh12345678"),
        line(r#"{"client_secret": "0123456789abcdef"}"#),
        line("git clone https://user:example-pass@example.com/repo.git"),
        line("Authorization: Bearer abcdefghijklmnopqrstuvwxyz"),
        line("max_tokens = 4096"),
        line("    Finished `dev` profile in 4.21s"),
    ]);
    assert_eq!(
        texts[0],
        format!("export EXAMPLE_API_KEY={}", dots(key.len()))
    );
    assert_eq!(texts[1], format!("API_TOKEN={}", dots(16)));
    assert_eq!(texts[2], format!(r#"{{"client_secret": "{}"}}"#, dots(16)));
    assert_eq!(
        texts[3],
        format!("git clone https://user:{}@example.com/repo.git", dots(12))
    );
    assert_eq!(texts[4], format!("Authorization: Bearer {}", dots(26)));
    assert_eq!(texts[5], "max_tokens = 4096");
    assert_eq!(texts[6], "    Finished `dev` profile in 4.21s");
}

#[test]
fn a_token_that_wrapped_is_masked_on_both_rows() {
    let token = format!("ghp_{}", "b".repeat(36));
    let mut first = line(&format!("token is {}", &token[..20]));
    first.set_last_cell_was_wrapped(true, SEQ_ZERO);
    let texts = masked(vec![first, line(&token[20..])]);
    assert_eq!(texts, vec![format!("token is {}", dots(20)), dots(20)]);
}

#[test]
fn a_private_key_is_masked_between_its_markers() {
    let texts = masked(vec![
        line("-----BEGIN OPENSSH PRIVATE KEY-----"),
        line("b3BlbnNzaC1rZXktdjEAAAAA"),
        line("QyNTUxOQAAACDx+8KxZ/ab=="),
        line("-----END OPENSSH PRIVATE KEY-----"),
        line("after"),
    ]);
    assert_eq!(
        texts,
        vec![
            "-----BEGIN OPENSSH PRIVATE KEY-----".to_string(),
            dots(24),
            dots(24),
            "-----END OPENSSH PRIVATE KEY-----".to_string(),
            "after".to_string(),
        ]
    );
}

/// A search hit or a diff shows the BEGIN line alone: nothing after it is
/// a key's body. Nor after code that quotes the marker.
#[test]
fn a_begin_line_without_its_body_masks_nothing_after_it() {
    let rows = vec![
        "id_rsa:-----BEGIN OPENSSH PRIVATE KEY-----",
        "notes.txt: nothing to see here",
        "$ ls",
        r#"let pem = "-----BEGIN RSA PRIVATE KEY-----";"#,
        "Finished",
    ];
    let texts = masked(rows.iter().map(|row| line(row)).collect());
    assert_eq!(texts, rows);
}

/// A listing that numbers its lines, a diff and a search with context put
/// a margin before each row of a key; the body past it is masked, the
/// margin kept.
#[test]
fn a_private_key_shown_with_a_margin_is_masked() {
    let body = "MIIEowIBAAKCAQEAx+8KxZab";
    let listings = [
        // bat
        vec![
            "   1 │ -----BEGIN OPENSSH PRIVATE KEY-----".to_string(),
            format!("   2 │ {body}"),
            format!("   3 │ {body}"),
            "   4 │ -----END OPENSSH PRIVATE KEY-----".to_string(),
        ],
        // cat -n, its tab put on screen as spaces
        vec![
            "     1  -----BEGIN RSA PRIVATE KEY-----".to_string(),
            format!("     2  {body}"),
            format!("     3  {body}"),
            "     4  -----END RSA PRIVATE KEY-----".to_string(),
        ],
        // A diff that removes it.
        vec![
            "------BEGIN RSA PRIVATE KEY-----".to_string(),
            format!("-{body}"),
            format!("-{body}"),
            "------END RSA PRIVATE KEY-----".to_string(),
        ],
        // grep -n -A, past line 9.
        vec![
            "key.pem:8:-----BEGIN RSA PRIVATE KEY-----".to_string(),
            format!("key.pem-9-{body}"),
            format!("key.pem-10-{body}"),
            "key.pem-11------END RSA PRIVATE KEY-----".to_string(),
        ],
    ];
    for rows in listings {
        let texts = masked(rows.iter().map(|row| line(row)).collect());
        for row in [1, 2] {
            let margin = rows[row].strip_suffix(body).unwrap();
            assert_eq!(texts[row], format!("{margin}{}", dots(body.len())));
        }
        assert_eq!(texts[0], rows[0]);
        assert_eq!(texts[3], rows[3]);
    }

    // A PGP key's header and blank line behind the margin too.
    let texts = masked(vec![
        line("   1 │ -----BEGIN PGP PRIVATE KEY BLOCK-----"),
        line("   2 │ Version: GnuPG v2"),
        line("   3 │"),
        line(&format!("   4 │ {body}")),
        line("   5 │ -----END PGP PRIVATE KEY BLOCK-----"),
    ]);
    assert_eq!(texts[1], "   2 │ Version: GnuPG v2");
    assert_eq!(texts[3], format!("   4 │ {}", dots(body.len())));
}

#[test]
fn common_secret_formats_are_masked() {
    let one_line_key = r"\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\n";
    let stripe = format!("sk_live_{}", "a1".repeat(12));
    let cases = [
        ("DB_PASS=hunter22", format!("DB_PASS={}", dots(8))),
        ("MYSQL_PWD=abcd", format!("MYSQL_PWD={}", dots(4))),
        ("PASSWORD=hunter2", format!("PASSWORD={}", dots(7))),
        (
            "mysql --password Sup3rS3cretPw",
            format!("mysql --password {}", dots(13)),
        ),
        (
            &*format!("STRIPE_KEY={}", stripe),
            format!("STRIPE_KEY={}", dots(stripe.len())),
        ),
        (
            &*format!(
                r#"  "private_key": "-----BEGIN PRIVATE KEY-----{}-----END PRIVATE KEY-----","#,
                one_line_key
            ),
            format!(
                r#"  "private_key": "-----BEGIN PRIVATE KEY-----{}-----END PRIVATE KEY-----","#,
                dots(one_line_key.len())
            ),
        ),
        (
            "tls.key: LS0tLS1CRUdJTiBQUklWQVRFIEtFWS0tLS0t",
            format!("tls.key: {}", dots(36)),
        ),
        (
            "Authorization: Basic dXNlcjpwYXNzd29yZA==",
            format!("Authorization: Basic {}", dots(20)),
        ),
    ];
    for (text, expected) in cases {
        assert_eq!(masked(vec![line(text)]), vec![expected], "{}", text);
    }
}

/// The names tools read a password from, with no separator before the
/// word, and passwords with the symbols generated ones have.
#[test]
fn passwords_by_those_names_and_with_symbols_are_masked() {
    let cases = [
        (
            "export PGPASSWORD=hunter22",
            format!("export PGPASSWORD={}", dots(8)),
        ),
        ("SSHPASS=correcthorse", format!("SSHPASS={}", dots(12))),
        ("DBPASS=s3cr3t", format!("DBPASS={}", dots(6))),
        (
            "DB_PASSWORD=Tr0ub4dor&3",
            format!("DB_PASSWORD={}", dots(11)),
        ),
        (
            "export DB_PASS=p@ss$word",
            format!("export DB_PASS={}", dots(9)),
        ),
        (
            "mysql --password 'p@ss(w)rd'",
            format!("mysql --password '{}'", dots(9)),
        ),
        (
            r#"  "password": "x(9]Kq*Lz","#,
            format!(r#"  "password": "{}","#, dots(9)),
        ),
        // In quotes, all of it: `,`, `;` and spaces too, and a `$` that
        // single quotes keep from being expanded.
        (
            "DB_PASSWORD='ab;Xk9!q2'",
            format!("DB_PASSWORD='{}'", dots(9)),
        ),
        (
            r#"  "password": "Tr0ub,4dor","#,
            format!(r#"  "password": "{}","#, dots(10)),
        ),
        (
            r#"export PASSPHRASE="correct horse battery""#,
            format!(r#"export PASSPHRASE="{} {} {}""#, dots(7), dots(5), dots(7)),
        ),
        (
            "sshpass -p x --password 'ab cd;ef'",
            format!("sshpass -p x --password '{} {}'", dots(2), dots(5)),
        ),
        ("PASSWORD='$ecr3tPw'", format!("PASSWORD='{}'", dots(8))),
    ];
    for (text, expected) in cases {
        assert_eq!(masked(vec![line(text)]), vec![expected], "{}", text);
    }
}

/// A client that fills in a command made only to carry the name -- with
/// the shell chosen in Settings, which the session server does not know --
/// has the spawn run it as it is.
#[test]
fn a_bare_command_a_client_filled_in_is_run_as_it_is() {
    let root = tempfile::tempdir().unwrap();
    let key = "thinkterm:project-1:thread-2";
    write_records(root.path(), Owner::Server, key, &[(3, "old".into())], 7).unwrap();
    let mut command = with_seed_at(root.path(), Owner::Server, key, 3, None);
    let given = command.as_mut().unwrap();
    given.env("SHELL", "/usr/local/bin/fish");
    filled_in(given);
    assert_eq!(
        take_named(&mut command).unwrap(),
        format!("server:3:{}", key)
    );
    let command = command.unwrap();
    assert!(command.is_default_prog());
    assert_eq!(command.get_env("SHELL").unwrap(), "/usr/local/bin/fish");
    assert!(command.get_env(SEED_ENV).is_none());
    assert!(command.get_env(BARE_SEED_ENV).is_none());
}

/// An encrypted PEM key and a PGP one carry header lines, and a blank line
/// before the body; the body is masked all the same.
#[test]
fn a_private_key_with_headers_is_masked() {
    let texts = masked(vec![
        line("-----BEGIN RSA PRIVATE KEY-----"),
        line("Proc-Type: 4,ENCRYPTED"),
        line("DEK-Info: AES-128-CBC,0123456789ABCDEF"),
        line(""),
        line("MIIEowIBAAKCAQEAx+8KxZab"),
        line("-----END RSA PRIVATE KEY-----"),
        line("-----BEGIN PGP PRIVATE KEY BLOCK-----"),
        line("Version: GnuPG v2"),
        line(""),
        line("lQOYBGT4Kx0BCAC7v9QeWcU1"),
        line("=Ab12"),
        line("-----END PGP PRIVATE KEY BLOCK-----"),
        line("after"),
    ]);
    assert_eq!(texts[4], dots(24));
    assert_eq!(texts[9], dots(24));
    assert_eq!(texts[10], dots(5));
    assert_eq!(texts[1], "Proc-Type: 4,ENCRYPTED");
    assert_eq!(texts[11], "-----END PGP PRIVATE KEY BLOCK-----");
    assert_eq!(texts[12], "after");
}

/// Code names things token and password too.
#[test]
fn code_that_mentions_secrets_is_left_alone() {
    let rows = vec![
        "let token = generate_token();",
        "fn parse(tokens: Vec<Token>) -> Result<()>",
        "pub secret_store: SecretStore,",
        "tokenizer.json: 100%|######| 2.11M/2.11M",
        "password: ${DB_PASSWORD}",
        "max_tokens = 4096",
        "password = getpass()",
        "PGPASSWORD=$PGPASS psql",
        "export DB_PASSWORD=$(cat secret.txt)",
        "let token = sha256(data);",
        "access_token = response.json()[0]",
        r#"PGPASSWORD="$DB_PASS" psql"#,
        "password = os.environ['DB_PASSWORD']",
        r#"log("password: %s, user: %s", p, u)"#,
    ];
    let texts = masked(rows.iter().map(|row| line(row)).collect());
    assert_eq!(texts, rows);
}

/// A link's address would put the secret back on disk however its text
/// was masked.
#[test]
fn links_that_carry_a_secret_are_dropped() {
    let url = "http://localhost:8888/lab?token=4f3a9c0d1e2f3a4b";
    let mut attrs = CellAttributes::default();
    attrs.set_hyperlink(Some(Arc::new(termwiz::hyperlink::Hyperlink::new(url))));
    let mut lines = vec![Line::from_text(url, &attrs, SEQ_ZERO, None)];
    mask_secrets(&mut lines);
    assert_eq!(
        text_of(&lines[0]),
        format!("http://localhost:8888/lab?token={}", dots(16))
    );
    assert!(lines[0]
        .visible_cells()
        .all(|cell| cell.attrs().hyperlink().is_none()));
    assert!(!render(&lines).unwrap().contains("4f3a9c0d"));
}

#[test]
fn masking_keeps_the_colors() {
    let mut attrs = CellAttributes::default();
    attrs.set_foreground(GREEN);
    let mut lines = vec![Line::from_text(
        "PASSWORD=correcthorse",
        &attrs,
        SEQ_ZERO,
        None,
    )];
    mask_secrets(&mut lines);
    assert_eq!(text_of(&lines[0]), format!("PASSWORD={}", dots(12)));
    assert!(lines[0].visible_cells().all(|cell| *cell.attrs() == attrs));
}

/// `text` without its colors.
fn plain(text: &str) -> String {
    Regex::new("\x1b\\[[0-9;:]*m")
        .unwrap()
        .replace_all(text, "")
        .into_owned()
}

#[test]
fn trailing_blank_rows_go_and_an_empty_screen_keeps_nothing() {
    assert_eq!(keep(vec![line(""), line("   ")]), None);
    let kept = keep(vec![line("$ ls"), line("README.md"), line(""), line("  ")]).unwrap();
    assert_eq!(plain(&kept), "$ ls\r\nREADME.md");
}

fn terminal(rows: usize, cols: usize) -> Terminal {
    Terminal::new(
        TerminalSize {
            rows,
            cols,
            ..TerminalSize::default()
        },
        Arc::new(config::TermConfig::new()),
        "ThinkTerm",
        "test",
        Box::new(std::io::sink()),
    )
}

fn screen_text(term: &Terminal) -> Vec<String> {
    let screen = term.screen_for_alt(false);
    screen
        .lines_in_phys_range(0..screen.scrollback_rows())
        .iter()
        .map(|line| text_of(line).trim_end().to_string())
        .collect()
}

/// After a row of plain spaces, as `git diff` prints for an empty context
/// line, the next row is plain again, not in the colour before.
#[test]
fn colours_end_where_they_ended() {
    let mut wide = terminal(4, 20);
    wide.advance_bytes("\x1b[31m-foo\x1b[0m\r\n \r\n bar");
    let kept = keep(wide.screen_for_alt(false).lines_in_phys_range(0..4)).unwrap();
    let mut back = terminal(4, 20);
    back.advance_bytes(&kept);
    let lines = back.screen_for_alt(false).lines_in_phys_range(0..3);
    let first = |row: usize| {
        lines[row]
            .visible_cells()
            .next()
            .unwrap()
            .attrs()
            .foreground()
    };
    assert_eq!(first(0), ColorAttribute::PaletteIndex(1));
    assert_eq!(text_of(&lines[2]).trim_end(), " bar");
    assert_eq!(first(2), ColorAttribute::Default);
}

#[test]
fn kept_text_reads_back_and_rewraps_at_the_new_width() {
    let mut wide = terminal(4, 20);
    wide.advance_bytes("\x1b[32mgreen\x1b[0m plain\r\n0123456789abcdefghijKLMN");
    let kept = keep(wide.screen_for_alt(false).lines_in_phys_range(0..4)).unwrap();

    let mut narrow = terminal(6, 10);
    narrow.advance_bytes(&kept);
    assert_eq!(
        screen_text(&narrow),
        vec!["green plai", "n", "0123456789", "abcdefghij", "KLMN", ""]
    );
    let green = narrow.screen_for_alt(false).lines_in_phys_range(0..1)[0]
        .visible_cells()
        .next()
        .unwrap()
        .attrs()
        .foreground();
    assert_eq!(green, GREEN);
}

/// A row that ends in a background, as `git diff` marks trailing
/// whitespace, paints only itself: the row put back after it scrolls in
/// plain, and is plain past its text.
#[test]
fn a_row_ending_in_a_background_does_not_paint_the_next() {
    let mut wide = terminal(4, 20);
    wide.advance_bytes("+foo\x1b[41m  \x1b[0m\r\nnext");
    let kept = keep(wide.screen_for_alt(false).lines_in_phys_range(0..4)).unwrap();
    // One row tall, so every line feed scrolls and fills the new row with
    // the pen of the moment.
    let mut back = terminal(1, 20);
    back.advance_bytes(&kept);
    let screen = back.screen_for_alt(false);
    let lines = screen.lines_in_phys_range(0..screen.scrollback_rows());
    let next = lines
        .iter()
        .find(|line| text_of(line).starts_with("next"))
        .unwrap();
    assert!(next
        .visible_cells()
        .all(|cell| cell.attrs().background() == ColorAttribute::Default));
}

/// A row that is only a background, as a diff viewer paints an empty added
/// line, leaves the row after it plain too.
#[test]
fn a_row_of_only_a_background_does_not_paint_the_next() {
    let mut wide = terminal(4, 20);
    wide.advance_bytes("\x1b[42m\x1b[K\x1b[0m\r\nnext");
    let kept = keep(wide.screen_for_alt(false).lines_in_phys_range(0..4)).unwrap();
    let mut back = terminal(1, 20);
    back.advance_bytes(&kept);
    let screen = back.screen_for_alt(false);
    let lines = screen.lines_in_phys_range(0..screen.scrollback_rows());
    let next = lines
        .iter()
        .find(|line| text_of(line).starts_with("next"))
        .unwrap();
    assert!(next
        .visible_cells()
        .all(|cell| cell.attrs().background() == ColorAttribute::Default));
    let filled = lines
        .iter()
        .find(|line| line.visible_cells().next().is_some())
        .unwrap();
    assert_eq!(
        filled.visible_cells().next().unwrap().attrs().background(),
        GREEN
    );
}

/// Where a long line wrapped on a space, the space is part of the text.
#[test]
fn a_space_where_a_line_wrapped_is_kept() {
    let mut narrow = terminal(4, 5);
    narrow.advance_bytes("aaaa bbbb cccc");
    let kept = keep(narrow.screen_for_alt(false).lines_in_phys_range(0..4)).unwrap();
    let mut wide = terminal(2, 20);
    wide.advance_bytes(&kept);
    assert_eq!(screen_text(&wide)[0], "aaaa bbbb cccc");
}

/// A wide character that did not fit at the end of a row went to the next
/// one; the cell it left empty is not text.
#[test]
fn the_cell_a_wide_character_left_at_a_wrap_is_not_kept() {
    let mut narrow = terminal(4, 5);
    narrow.advance_bytes("abcd漢字");
    let kept = keep(narrow.screen_for_alt(false).lines_in_phys_range(0..4)).unwrap();
    let mut wide = terminal(2, 20);
    wide.advance_bytes(&kept);
    assert_eq!(screen_text(&wide)[0], "abcd漢字");
}

#[test]
fn a_seed_ends_with_the_separator_and_under_a_pseudo_console_moves_up() {
    let seed = Seed {
        closed: now(),
        text: "old".into(),
    };
    let plain = String::from_utf8(seed.bytes(3, false)).unwrap();
    assert!(
        plain.starts_with("old\r\n\u{1b}[2m── closed "),
        "{:?}",
        plain
    );
    assert!(plain.ends_with(" ──\u{1b}[0m\r\n"), "{:?}", plain);

    let conpty = String::from_utf8(seed.bytes(3, true)).unwrap();
    assert!(
        conpty.ends_with(" ──\u{1b}[0m\r\n\r\n\r\n\u{1b}[H"),
        "{:?}",
        conpty
    );
    // The separator is the last row above the screen; the screen is empty
    // and the shell starts at its top.
    let mut term = terminal(3, 60);
    term.advance_bytes(&conpty);
    let text = screen_text(&term);
    assert_eq!(&text[text.len() - 3..], &["", "", ""]);
    assert!(text[text.len() - 4].starts_with("── closed "), "{:?}", text);
    assert_eq!(term.cursor_pos().y, 0);
}

#[test]
fn a_spawn_names_its_record_and_takes_it_off_the_disk() {
    let root = tempfile::tempdir().unwrap();
    let key = "thinkterm:project-1:thread-2";
    write_records(root.path(), Owner::Server, key, &[(3, "old".into())], 7).unwrap();
    // Nothing kept for pane 4: its command stays as it was.
    assert!(with_seed_at(root.path(), Owner::Server, key, 4, None).is_none());

    let mut command = with_seed_at(root.path(), Owner::Server, key, 3, None);
    let named = take_named(&mut command).unwrap();
    assert_eq!(named, format!("server:3:{}", key));
    // It gave no command, and is spawned as giving none.
    assert!(command.is_none());
    // One that gave a command keeps it, without the name.
    let mut command = with_seed_at(
        root.path(),
        Owner::Server,
        key,
        3,
        Some(CommandBuilder::new("htop")),
    );
    assert_eq!(take_named(&mut command).unwrap(), named);
    let command = command.unwrap();
    assert_eq!(command.get_argv(), &vec![std::ffi::OsString::from("htop")]);
    assert!(command.get_env(SEED_ENV).is_none());
    assert!(command.get_env(BARE_SEED_ENV).is_none());
    let claim = take_seed_at(root.path(), &named).unwrap();
    assert_eq!(
        claim.seed,
        Seed {
            closed: 7,
            text: "old".into()
        }
    );
    // While its pane is being made, no other pane gets it.
    assert!(take_seed_at(root.path(), &named).is_none());
    // A spawn that failed leaves it for the next try.
    drop(claim);
    let claim = take_seed_at(root.path(), &named).unwrap();
    // Spent once the pane exists: the next pane with that id gets nothing.
    claim.spend();
    assert!(take_seed_at(root.path(), &named).is_none());
    let dir = key_dir(root.path(), Owner::Server, key);
    assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
    for nonsense in ["", "other:3:key", "server:three:key", "server:3"] {
        assert!(
            take_seed_at(root.path(), nonsense).is_none(),
            "{}",
            nonsense
        );
    }
}

#[test]
fn a_thread_without_a_layout_takes_only_a_lone_record() {
    let root = tempfile::tempdir().unwrap();
    assert!(with_lone_seed_at(root.path(), "default", None).is_none());

    write_records(
        root.path(),
        Owner::Server,
        "default",
        &[(5, "one".into())],
        1,
    )
    .unwrap();
    let mut command = with_lone_seed_at(root.path(), "default", None);
    assert_eq!(take_named(&mut command).unwrap(), "server:5:default");

    // Two panes kept: no telling which one the lone new pane was.
    write_records(
        root.path(),
        Owner::Server,
        "default",
        &[(5, "one".into()), (6, "two".into())],
        2,
    )
    .unwrap();
    assert!(with_lone_seed_at(root.path(), "default", None).is_none());
}

#[test]
fn a_restored_layout_spends_what_was_kept_for_it() {
    let root = tempfile::tempdir().unwrap();
    let kept = vec![(3, "one".to_string()), (7, "two\r\nlines".to_string())];
    for owner in [Owner::Server, Owner::App] {
        write_records(root.path(), owner, "default", &kept, 1_700_000_000).unwrap();
    }
    write_records(root.path(), Owner::Server, "other", &kept, 1).unwrap();
    assert_eq!(
        take_seed_at(root.path(), "app:7:default")
            .unwrap()
            .seed
            .text,
        "two\r\nlines"
    );
    finish_at(root.path(), "default");
    for owner in [Owner::Server, Owner::App] {
        assert!(!key_dir(root.path(), owner, "default").exists());
    }
    assert!(key_dir(root.path(), Owner::Server, "other").exists());
}

#[test]
fn nothing_to_keep_leaves_the_last_record_and_new_ones_replace_it() {
    let root = tempfile::tempdir().unwrap();
    let dir = key_dir(root.path(), Owner::App, "thinkterm:project-1:thread-2");
    assert!(dir.ends_with("app/thinkterm_project-1_thread-2"));

    write_records(
        root.path(),
        Owner::App,
        "thinkterm:project-1:thread-2",
        &[(1, "old".into())],
        1,
    )
    .unwrap();
    write_records(
        root.path(),
        Owner::App,
        "thinkterm:project-1:thread-2",
        &[],
        2,
    )
    .unwrap();
    assert!(dir.join("1").exists());

    write_records(
        root.path(),
        Owner::App,
        "thinkterm:project-1:thread-2",
        &[(2, "new".into())],
        3,
    )
    .unwrap();
    assert!(!dir.join("1").exists());
    assert_eq!(
        take_seed_at(root.path(), "app:2:thinkterm:project-1:thread-2")
            .unwrap()
            .seed
            .text,
        "new"
    );
}

#[test]
fn keys_cannot_leave_the_owners_folder() {
    let root = Path::new("/home/user/kept");
    for key in ["..", ".", "", "../other", "a/b", "C:x"] {
        let dir = key_dir(root, Owner::Server, key);
        assert_eq!(dir.parent(), Some(root.join("server").as_path()), "{key}");
        assert!(!dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .chars()
            .all(|c| c == '.'));
    }
}

#[test]
fn prune_removes_unknown_and_old_records() {
    let root = tempfile::tempdir().unwrap();
    for key in ["keep", "gone"] {
        write_records(root.path(), Owner::Server, key, &[(1, "x".into())], 1).unwrap();
    }
    let keep: HashSet<String> = ["keep".to_string()].into();
    prune_at(root.path(), Owner::Server, &keep, SystemTime::now());
    assert!(key_dir(root.path(), Owner::Server, "keep").exists());
    assert!(!key_dir(root.path(), Owner::Server, "gone").exists());

    let later = SystemTime::now() + MAX_AGE + Duration::from_secs(24 * 60 * 60);
    prune_at(root.path(), Owner::Server, &keep, later);
    assert!(!key_dir(root.path(), Owner::Server, "keep").exists());

    // Old records go whoever kept them: the other process may never run
    // here again to clear its own.
    write_records(root.path(), Owner::App, "elsewhere", &[(1, "x".into())], 1).unwrap();
    prune_stale_at(root.path(), SystemTime::now());
    assert!(key_dir(root.path(), Owner::App, "elsewhere").exists());
    prune_stale_at(root.path(), later);
    assert!(!key_dir(root.path(), Owner::App, "elsewhere").exists());
}

#[test]
fn the_switch_is_a_file_that_says_on() {
    let dir = tempfile::tempdir().unwrap();
    let switch = dir.path().join("thinkterm").join("restore-pane-output");
    assert!(!enabled_at(&switch));
    set_enabled_at(&switch, true).unwrap();
    assert!(enabled_at(&switch));
    assert_eq!(std::fs::read_to_string(&switch).unwrap(), "on\n");
    set_enabled_at(&switch, false).unwrap();
    assert!(!enabled_at(&switch));
    assert_eq!(std::fs::read_to_string(&switch).unwrap(), "off\n");
    // Off twice is still off; anything but "on" is off.
    set_enabled_at(&switch, false).unwrap();
    std::fs::write(&switch, "yes").unwrap();
    assert!(!enabled_at(&switch));
}

#[test]
fn the_desktop_turns_on_only_a_switch_never_set() {
    let dir = tempfile::tempdir().unwrap();
    let switch = dir.path().join("thinkterm").join("restore-pane-output");
    on_by_default_at(&switch).unwrap();
    assert!(enabled_at(&switch));
    set_enabled_at(&switch, false).unwrap();
    on_by_default_at(&switch).unwrap();
    assert!(!enabled_at(&switch));
}
