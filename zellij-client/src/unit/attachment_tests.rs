use crate::explicit_attach_default_mode;
use zellij_utils::{
    cli::{CliArgs, Command, SessionCommand, Sessions},
    data::InputMode,
    input::options::Options,
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
        }
    }
}
