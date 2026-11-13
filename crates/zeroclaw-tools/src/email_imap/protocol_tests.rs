//! Wire-level regressions for the production helpers used by email_read/search.
//! The peer is an in-memory byte stream: these are protocol/parser regressions,
//! not live TLS, credential-service or provider acceptance tests. IDLE is an
//! upstream pin-projection regression, not a new application capability.

use std::collections::HashSet;
use std::future::Future;
use std::io::ErrorKind;
use std::time::Duration;

use async_imap::error::Error;
use async_imap::extensions::idle::IdleResponse;
use async_imap::imap_proto::{MailboxDatum, Response, Status};
use async_imap::{Authenticator, Client};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

use super::{PeekPart, uid_fetch_peek, uid_search};

type Step = (&'static str, String);

fn login() -> Step {
    (
        "LOGIN \"reader@example.test\" \"fixture-password\"",
        // An unrelated tagged NO must not fail the current LOGIN. Unicode in
        // greetings and tagged outcomes also exercises imap-proto 0.17.
        "OLD42 NO unrelated request\r\n{tag} OK logged in – ready\r\n".into(),
    )
}

fn examine() -> Step {
    (
        "EXAMINE \"INBOX\"",
        "* FLAGS (\\Seen)\r\n* 2 EXISTS\r\n{tag} OK [READ-ONLY] EXAMINE completed\r\n".into(),
    )
}

fn logout() -> Step {
    (
        "LOGOUT",
        "* BYE logging out\r\n{tag} OK LOGOUT completed\r\n".into(),
    )
}

async fn with_peer<F, Fut>(steps: Vec<Step>, exercise: F)
where
    F: FnOnce(Client<DuplexStream>) -> Fut,
    Fut: Future<Output = ()>,
{
    // A small capacity plus three-byte writes forces Pending and fragmented
    // reads, including splits inside UTF-8 and IMAP literals.
    let (client_stream, server_stream) = tokio::io::duplex(16);
    let peer = async move {
        let mut stream = BufReader::new(server_stream);
        for chunk in "* OK IMAP fixture – ready\r\n".as_bytes().chunks(3) {
            stream.get_mut().write_all(chunk).await.unwrap();
        }
        let mut tag = String::new();
        let mut request_tags = HashSet::new();
        for (expected, response) in steps {
            let mut line = String::new();
            assert_ne!(
                stream.read_line(&mut line).await.unwrap(),
                0,
                "missing {expected}"
            );
            assert!(line.ends_with("\r\n"), "unterminated command: {line:?}");
            let command = line.trim_end_matches("\r\n");
            // DONE and the SASL response are untagged and finish the most
            // recent tagged request; every other command has its own tag.
            let actual = if let Some((next_tag, command)) = command.split_once(' ') {
                assert!(
                    request_tags.insert(next_tag.to_owned()),
                    "reused request tag"
                );
                tag = next_tag.to_owned();
                command
            } else {
                command
            };
            assert_eq!(actual, expected);
            let response = response.replace("{tag}", &tag);
            for chunk in response.as_bytes().chunks(3) {
                stream.get_mut().write_all(chunk).await.unwrap();
            }
        }
        stream.get_mut().shutdown().await.unwrap();
    };
    let client = async move {
        let mut client = Client::new(client_stream);
        let greeting = client.read_response().await.unwrap().unwrap();
        assert!(matches!(
            greeting.parsed(),
            Response::Data {
                status: Status::Ok,
                ..
            }
        ));
        exercise(client).await;
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(peer, client);
    })
    .await
    .expect("IMAP fixture did not complete");
}

#[tokio::test]
async fn search_and_read_decode_fragmented_peek_literals() {
    const HEADER: &str = "From: Fixture <sender@example.test>\r\nSubject: Protocol fixture\r\n\r\n";
    const BODY: &str =
        "From: sender@example.test\r\nSubject: Protocol fixture\r\n\r\nMessage with UTF-8: λ\r\n";
    with_peer(
        vec![
            login(),
            examine(),
            (
                "UID SEARCH UNSEEN",
                "* 3 EXISTS\r\n* SEARCH 7 11\r\nOLD42 NO unrelated search\r\n{tag} OK SEARCH completed\r\n".into(),
            ),
            (
                "UID FETCH 7,11 BODY.PEEK[HEADER]",
                format!(
                    "* 1 FETCH (FLAGS (\\Seen))\r\n\
                 * 3 FETCH (UID 99 BODY[HEADER] {{4}}\r\njunk)\r\n\
                 * 1 FETCH (UID 7 BODY[HEADER] {{{}}}\r\n{})\r\n\
                 * 2 FETCH (UID 11 BODY[HEADER] {{{}}}\r\n{})\r\n\
                 OLD42 BAD unrelated fetch\r\n\
                 {{tag}} OK FETCH completed\r\n",
                    HEADER.len(),
                    HEADER,
                    HEADER.len(),
                    HEADER,
                ),
            ),
            (
                "UID FETCH 11 BODY.PEEK[]",
                format!(
                    "* 1 FETCH (UID 7 BODY[] {{4}}\r\njunk)\r\n\
                     * 2 FETCH (UID 11 FLAGS (\\Seen))\r\n\
                     * 2 FETCH (UID 11 BODY[] {{{}}}\r\n{})\r\n{{tag}} OK FETCH completed\r\n",
                    BODY.len(),
                    BODY,
                ),
            ),
            logout(),
        ],
        |client| async move {
            let mut session = client
                .login("reader@example.test", "fixture-password")
                .await
                .unwrap();
            assert_eq!(session.examine("INBOX").await.unwrap().exists, 2);
            assert_eq!(
                uid_search(&mut session, "UNSEEN").await.unwrap(),
                HashSet::from([7, 11])
            );
            let headers = uid_fetch_peek(&mut session, &[7, 11], PeekPart::Header)
                .await
                .unwrap();
            assert_eq!(headers.len(), 2);
            assert_eq!(headers[0].uid, 7);
            assert_eq!(headers[1].uid, 11);
            assert!(
                headers
                    .iter()
                    .all(|message| message.data.as_deref() == Some(HEADER.as_bytes()))
            );
            let bodies = uid_fetch_peek(&mut session, &[11], PeekPart::Body)
                .await
                .unwrap();
            assert_eq!(bodies.len(), 1);
            assert_eq!(bodies[0].uid, 11);
            assert_eq!(bodies[0].data.as_deref(), Some(BODY.as_bytes()));
            session.logout().await.unwrap();
        },
    )
    .await;
}

#[tokio::test]
async fn zero_search_limit_does_not_send_an_empty_fetch() {
    with_peer(vec![login(), examine(), logout()], |client| async move {
        let mut session = client
            .login("reader@example.test", "fixture-password")
            .await
            .unwrap();
        session.examine("INBOX").await.unwrap();
        assert!(
            uid_fetch_peek(&mut session, &[], PeekPart::Header)
                .await
                .unwrap()
                .is_empty()
        );
        session.logout().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn matching_login_rejection_does_not_create_a_session() {
    with_peer(
        vec![(login().0, "{tag} NO authentication rejected\r\n".into())],
        |client| async move {
            let (error, _) = client
                .login("reader@example.test", "fixture-password")
                .await
                .unwrap_err();
            assert!(
                matches!(error, Error::No(message) if message.contains("authentication rejected"))
            );
        },
    )
    .await;
}

#[tokio::test]
async fn oauth2_challenge_and_completion_survive_fragmented_responses() {
    struct FixtureOAuth2;
    impl Authenticator for FixtureOAuth2 {
        type Response = &'static str;

        fn process(&mut self, challenge: &[u8]) -> Self::Response {
            assert!(challenge.is_empty());
            "user=reader@example.test\x01auth=Bearer fixture-token\x01\x01"
        }
    }

    with_peer(
        vec![
            ("AUTHENTICATE XOAUTH2", "+ \r\n".into()),
            (
                "dXNlcj1yZWFkZXJAZXhhbXBsZS50ZXN0AWF1dGg9QmVhcmVyIGZpeHR1cmUtdG9rZW4BAQ==",
                "{tag} OK authenticated\r\n".into(),
            ),
            logout(),
        ],
        |client| async move {
            let mut session = client.authenticate("XOAUTH2", FixtureOAuth2).await.unwrap();
            session.logout().await.unwrap();
        },
    )
    .await;
}

#[tokio::test]
async fn rejected_fetch_does_not_return_partial_success() {
    for status in ["NO", "BAD"] {
        with_peer(
            vec![
                login(), examine(),
                ("UID FETCH 7 BODY.PEEK[]", format!(
                    "* 1 FETCH (UID 7 BODY[] {{4}}\r\npart)\r\nOTHER99 OK unrelated request\r\n{{tag}} {status} fetch rejected\r\n"
                )),
            ],
            |client| async move {
                let mut session = client.login("reader@example.test", "fixture-password").await.unwrap();
                session.examine("INBOX").await.unwrap();
                let error = uid_fetch_peek(&mut session, &[7], PeekPart::Body)
                    .await.unwrap_err();
                match (status, error) {
                    ("NO", Error::No(message)) | ("BAD", Error::Bad(message)) => {
                        assert!(message.contains("fetch rejected"));
                    }
                    (_, error) => panic!("unexpected FETCH error: {error:?}"),
                }
            },
        ).await;
    }
}

#[tokio::test]
async fn truncated_fetch_literal_propagates_unexpected_eof() {
    with_peer(
        vec![
            login(),
            examine(),
            (
                "UID FETCH 7 BODY.PEEK[]",
                "* 1 FETCH (UID 7 BODY[] {128}\r\ntruncated".into(),
            ),
        ],
        |client| async move {
            let mut session = client
                .login("reader@example.test", "fixture-password")
                .await
                .unwrap();
            session.examine("INBOX").await.unwrap();
            let error = uid_fetch_peek(&mut session, &[7], PeekPart::Body)
                .await
                .unwrap_err();
            assert!(matches!(error, Error::Io(error) if error.kind() == ErrorKind::UnexpectedEof));
        },
    )
    .await;
}

#[tokio::test]
async fn rejected_search_does_not_return_partial_success() {
    for status in ["NO", "BAD"] {
        with_peer(
            vec![
                login(),
                examine(),
                (
                    "UID SEARCH ALL",
                    format!(
                        "* SEARCH 7 11\r\nOTHER99 OK unrelated request\r\n{{tag}} {status} search rejected\r\n"
                    ),
                ),
            ],
            |client| async move {
                let mut session = client
                    .login("reader@example.test", "fixture-password")
                    .await
                    .unwrap();
                session.examine("INBOX").await.unwrap();
                let error = uid_search(&mut session, "ALL").await.unwrap_err();
                match (status, error) {
                    ("NO", Error::No(message)) | ("BAD", Error::Bad(message)) => {
                        assert!(message.contains("search rejected"));
                    }
                    (_, error) => panic!("unexpected SEARCH error: {error:?}"),
                }
            },
        )
        .await;
    }
}

#[tokio::test]
async fn eof_before_completion_does_not_return_partial_success() {
    for fetch in [true, false] {
        let step = if fetch {
            (
                "UID FETCH 7 BODY.PEEK[]",
                "* 1 FETCH (UID 7 BODY[] {4}\r\npart)\r\n".into(),
            )
        } else {
            ("UID SEARCH ALL", "* SEARCH 7 11\r\n".into())
        };
        with_peer(vec![login(), examine(), step], |client| async move {
            let mut session = client
                .login("reader@example.test", "fixture-password")
                .await
                .unwrap();
            session.examine("INBOX").await.unwrap();
            let error = if fetch {
                uid_fetch_peek(&mut session, &[7], PeekPart::Body)
                    .await
                    .unwrap_err()
            } else {
                uid_search(&mut session, "ALL").await.unwrap_err()
            };
            assert!(matches!(error, Error::ConnectionLost));
        })
        .await;
    }
}

#[tokio::test]
async fn server_bye_does_not_return_partial_success() {
    with_peer(
        vec![
            login(),
            examine(),
            (
                "UID FETCH 7 BODY.PEEK[]",
                "* 1 FETCH (UID 7 BODY[] {4}\r\npart)\r\n* BYE shutting down\r\n".into(),
            ),
        ],
        |client| async move {
            let mut session = client
                .login("reader@example.test", "fixture-password")
                .await
                .unwrap();
            session.examine("INBOX").await.unwrap();
            let error = uid_fetch_peek(&mut session, &[7], PeekPart::Body)
                .await
                .unwrap_err();
            assert!(matches!(error, Error::ConnectionLost));
        },
    )
    .await;
}

#[tokio::test]
async fn idle_update_interrupt_and_done_restore_the_session() {
    with_peer(
        vec![
            login(),
            examine(),
            ("IDLE", "+ idling\r\n* 3 EXISTS\r\n".into()),
            ("DONE", "{tag} OK IDLE completed\r\n".into()),
            ("NOOP", "{tag} OK NOOP completed\r\n".into()),
            logout(),
        ],
        |client| async move {
            let mut session = client
                .login("reader@example.test", "fixture-password")
                .await
                .unwrap();
            session.examine("INBOX").await.unwrap();
            let mut idle = session.idle();
            idle.init().await.unwrap();
            {
                let (wait, _interrupt) = idle.wait_with_timeout(Duration::from_secs(1));
                let IdleResponse::NewData(response) = wait.await.unwrap() else {
                    panic!("IDLE did not deliver the mailbox update");
                };
                assert!(matches!(
                    response.parsed(),
                    Response::MailboxData(MailboxDatum::Exists(3))
                ));
            }
            {
                let (wait, interrupt) = idle.wait_with_timeout(Duration::from_secs(1));
                drop(interrupt);
                assert_eq!(wait.await.unwrap(), IdleResponse::ManualInterrupt);
            }
            let mut session = idle.done().await.unwrap();
            session.noop().await.unwrap();
            session.logout().await.unwrap();
        },
    )
    .await;
}
