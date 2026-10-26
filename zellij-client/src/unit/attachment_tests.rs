use crate::{explicit_attach_default_mode, startup_mode_override};
use zellij_utils::{
    cli::{CliArgs, Command, SessionCommand, Sessions},
    data::{BareKey, InputMode, KeyWithModifier},
    input::{actions::Action, keybinds::Keybinds, options::Options},
};

fn attachment(options: Option<Options>, detach_others: bool) -> CliArgs {
    CliArgs {
        command: Some(Command::Sessions(Sessions::Attach {
            session_name: Some("work".to_owned()),
            detach_others,
            create: false,
            create_background: false,
            index: None,
            options: options.map(|options| Box::new(SessionCommand::Options(options))),
            force_run_commands: false,
            token: None,
            remember: false,
            forget: false,
            ca_cert: None,
            insecure: false,
            initial_command: vec![],
            close_on_exit: false,
            start_suspended: false,
        })),
        ..CliArgs::default()
    }
}

// LEGACY_COMPAT(next-major): plain native attach must retain its usual behavior.
#[test]
fn only_explicit_attachment_mode_requests_a_startup_override() {
    for cli in [
        attachment(None, false),
        attachment(Some(Options::default()), false),
        CliArgs {
            command: Some(Command::Options(Options {
                default_mode: Some(InputMode::Locked),
                ..Options::default()
            })),
            ..CliArgs::default()
        },
        CliArgs::default(),
    ] {
        assert_eq!(explicit_attach_default_mode(&cli), None);
        assert_eq!(startup_mode_override(&cli, true), None);
    }
}

// LEGACY_COMPAT(next-major): the older server's workaround preserves the
// explicit attachment mode, whether or not this client requests takeover.
#[test]
fn explicit_attachment_modes_work_with_and_without_takeover() {
    for takeover in [false, true] {
        for mode in [InputMode::Locked, InputMode::Normal] {
            let cli = attachment(
                Some(Options {
                    default_mode: Some(mode),
                    ..Options::default()
                }),
                takeover,
            );
            assert_eq!(explicit_attach_default_mode(&cli), Some(mode));
            assert_eq!(startup_mode_override(&cli, true), Some(mode));
        }
    }
}

#[test]
fn initial_mode_is_independent_of_fresh_and_attachment_base_modes() {
    for is_attachment in [false, true] {
        let mut cli = if is_attachment {
            attachment(
                Some(Options {
                    default_mode: Some(InputMode::Normal),
                    ..Options::default()
                }),
                false,
            )
        } else {
            CliArgs {
                command: Some(Command::Options(Options {
                    default_mode: Some(InputMode::Normal),
                    ..Options::default()
                })),
                ..CliArgs::default()
            }
        };
        cli.initial_mode = Some(InputMode::Locked);
        assert_eq!(
            startup_mode_override(&cli, is_attachment),
            Some(InputMode::Locked)
        );
        let base_mode = if is_attachment {
            explicit_attach_default_mode(&cli).unwrap()
        } else {
            cli.options().unwrap().default_mode.unwrap()
        };
        assert_eq!(base_mode, InputMode::Normal);

        // Normal forwards unbound keys to either the sidebar or the application.
        for (bare_key, bytes) in [
            (BareKey::Char('n'), b"n".to_vec()),
            (BareKey::Char('r'), b"r".to_vec()),
            (BareKey::Char('j'), b"j".to_vec()),
            (BareKey::Down, b"\x1b[B".to_vec()),
        ] {
            let key = KeyWithModifier::new(bare_key);
            for mode in [InputMode::Locked, InputMode::Normal] {
                assert_eq!(
                    Keybinds::default().get_actions_for_key_in_mode_or_default_action(
                        &mode,
                        &key,
                        bytes.clone(),
                        base_mode,
                        false,
                    ),
                    vec![Action::Write {
                        key_with_modifier: Some(key.clone()),
                        bytes: bytes.clone(),
                        is_kitty_keyboard_protocol: false,
                    }]
                );
            }
            assert_eq!(
                Keybinds::default().default_action_for_mode(
                    &InputMode::Pane,
                    Some(&key),
                    bytes,
                    base_mode,
                    false,
                ),
                Action::NoOp
            );
        }
    }
}

