use std::error::Error;
use std::io;

use computer_use_mcp::backend::{
    self, ActionKind, ActionRequest, CapturePolicy, MouseButton, ObserveRequest, RootFilters,
};

enum Operation {
    Observe,
    Click(String),
    ClickCenter,
    Type(String),
}

fn main() -> Result<(), Box<dyn Error>> {
    let (app, operation) = parse_args()?;
    let root = backend::list_roots(&RootFilters {
        app: Some(app.clone()),
        ..RootFilters::default()
    })?
    .into_iter()
    .next()
    .ok_or_else(|| io::Error::other(format!("no window matched app substring {app:?}")))?;

    println!(
        "frontmost_pid_before={:?}",
        computer_use_mcp::frontmost_pid()
    );
    let result = run_operation(&root, operation);
    println!(
        "frontmost_pid_after={:?}",
        computer_use_mcp::frontmost_pid()
    );
    result
}

fn run_operation(root: &backend::RootInfo, operation: Operation) -> Result<(), Box<dyn Error>> {
    match operation {
        Operation::Observe => {
            let mut observation = backend::observe(
                root,
                ObserveRequest {
                    semantic: true,
                    capture: CapturePolicy::Never,
                },
            )?;
            computer_use_mcp::outline::assign_refs(&mut observation.tree);
            println!(
                "{}",
                computer_use_mcp::outline::render_folded(&observation.tree)
            );
        }
        Operation::Click(ref_id) => {
            let mut observation = backend::observe(
                root,
                ObserveRequest {
                    semantic: true,
                    capture: CapturePolicy::Never,
                },
            )?;
            computer_use_mcp::outline::assign_refs(&mut observation.tree);
            let node = observation.tree.find(&ref_id).ok_or_else(|| {
                io::Error::other(format!(
                    "element {ref_id} was not present; run --observe to inspect current refs"
                ))
            })?;
            let path = computer_use_mcp::outline::path_to_ref(&observation.tree, &ref_id)
                .ok_or_else(|| io::Error::other(format!("could not resolve path for {ref_id}")))?;
            let request = ActionRequest {
                kind: ActionKind::Click,
                target_path: Some(path),
                target_frame: Some(node.frame),
                target_role: Some(node.role.clone()),
                target_title: Some(node.title.clone()),
                target_actions: node.actions.clone(),
                x: None,
                y: None,
                text: None,
                keys: None,
                scroll_x: None,
                scroll_y: None,
                path: None,
                button: MouseButton::Left,
                click_count: 1,
            };
            print_action_result(backend::perform_action(root, &request)?);
        }
        Operation::ClickCenter => {
            let (x, y) = root.frame.center();
            let request = coordinate_request(ActionKind::Click, Some(x), Some(y), None);
            print_action_result(backend::perform_action(root, &request)?);
        }
        Operation::Type(text) => {
            let request = coordinate_request(ActionKind::TypeText, None, None, Some(text));
            print_action_result(backend::perform_action(root, &request)?);
        }
    }
    Ok(())
}

fn coordinate_request(
    kind: ActionKind,
    x: Option<f64>,
    y: Option<f64>,
    text: Option<String>,
) -> ActionRequest {
    ActionRequest {
        kind,
        target_path: None,
        target_frame: None,
        target_role: None,
        target_title: None,
        target_actions: Vec::new(),
        x,
        y,
        text,
        keys: None,
        scroll_x: None,
        scroll_y: None,
        path: None,
        button: MouseButton::Left,
        click_count: 1,
    }
}

fn print_action_result(result: backend::ActionResult) {
    let json = serde_json::to_string_pretty(&result).unwrap_or_else(|_| format!("{result:?}"));
    println!("{json}");
}

fn parse_args() -> Result<(String, Operation), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mut app = None;
    let mut operation = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--app" => app = Some(next_value(&mut args, "--app")?),
            "--observe" => set_operation(&mut operation, Operation::Observe)?,
            "--click" => {
                let ref_id = next_value(&mut args, "--click")?;
                set_operation(&mut operation, Operation::Click(ref_id))?;
            }
            "--click-center" => set_operation(&mut operation, Operation::ClickCenter)?,
            "--type" => {
                let text = next_value(&mut args, "--type")?;
                set_operation(&mut operation, Operation::Type(text))?;
            }
            _ => return Err(usage(format!("unknown argument {argument:?}"))),
        }
    }
    let app = app.ok_or_else(|| usage("--app <substr> is required"))?;
    let operation = operation.ok_or_else(|| {
        usage("choose one of --observe, --click <ref>, --click-center, or --type <text>")
    })?;
    Ok((app, operation))
}

fn next_value(
    args: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<String, Box<dyn Error>> {
    args.next()
        .ok_or_else(|| usage(format!("{flag} requires a value")))
}

fn set_operation(slot: &mut Option<Operation>, value: Operation) -> Result<(), Box<dyn Error>> {
    if slot.replace(value).is_some() {
        Err(usage("only one probe operation may be selected"))
    } else {
        Ok(())
    }
}

fn usage(message: impl Into<String>) -> Box<dyn Error> {
    let message = message.into();
    io::Error::other(format!(
        "{message}\nusage: cu_probe --app <substr> (--observe | --click <ref> | --click-center | --type <text>)"
    ))
    .into()
}
