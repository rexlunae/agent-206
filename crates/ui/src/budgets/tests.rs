use super::*;
use a206_core::{CodingBackend, Mode};

fn thread(parent: Option<Id>, budget: ToolBudget) -> Thread {
    let mut thread = Thread::new("Budget test".into(), Mode::Code);
    thread.parent = parent;
    thread.backend = CodingBackend::Native;
    thread.tool_budget = budget;
    thread
}
#[test]
fn each_limit_uses_the_smallest_ceiling_in_the_whole_lineage() {
    let root = thread(
        None,
        ToolBudget {
            rounds: 2,
            tool_calls: 8,
            tokens: Some(91),
        },
    );
    let parent = thread(
        Some(root.id),
        ToolBudget {
            rounds: 9,
            tool_calls: 3,
            tokens: None,
        },
    );
    let child = thread(
        Some(parent.id),
        ToolBudget {
            rounds: 7,
            tool_calls: 5,
            tokens: Some(53),
        },
    );
    let id = child.id;
    assert_eq!(
        effective(&[root, parent, child], id),
        Some(ToolBudget {
            rounds: 2,
            tool_calls: 3,
            tokens: Some(53)
        })
    );
}
#[test]
fn missing_or_cyclic_ancestors_are_unknown_instead_of_unlimited() {
    let mut child = thread(Some(Id::new_v4()), ToolBudget::default());
    assert_eq!(effective(&[child.clone()], child.id), None);
    child.parent = Some(child.id);
    assert_eq!(effective(&[child.clone()], child.id), None);
    assert_eq!(effective(&[], child.id), None);
}
#[test]
fn budgets_accept_zero_calls_and_blank_tokens_but_refuse_invalid_limits() {
    assert_eq!(
        parse(" 1 ", "0", " "),
        Some(ToolBudget {
            rounds: 1,
            tool_calls: 0,
            tokens: None
        })
    );
    assert_eq!(parse("256", "256", "27").unwrap().tokens, Some(27));
    for (rounds, calls, tokens) in [
        ("0", "1", ""),
        ("257", "1", ""),
        ("1", "257", ""),
        ("1", "1", "0"),
        ("1.5", "1", ""),
        ("1", "-1", ""),
        ("1", "1", "18446744073709551616"),
    ] {
        assert!(
            parse(rounds, calls, tokens).is_none(),
            "{rounds}/{calls}/{tokens}"
        );
    }
}
#[test]
fn editor_renders_saved_and_inherited_limits_and_disables_offline_changes() {
    #[derive(Props, Clone, PartialEq)]
    struct Shown {
        online: bool,
        inherited: Option<ToolBudget>,
    }
    fn root(props: Shown) -> Element {
        rsx! { BudgetEditor { current: ToolBudget { rounds: 8, tool_calls: 32, tokens: None }, inherited: props.inherited, online: props.online, busy: false, onsave: move |_| {} } }
    }
    let mut dom = VirtualDom::new_with_props(
        root,
        Shown {
            online: false,
            inherited: Some(ToolBudget {
                rounds: 3,
                tool_calls: 0,
                tokens: Some(29),
            }),
        },
    );
    dom.rebuild_in_place();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("Saved here:"));
    assert!(html.contains("3 model rounds · 0 tool calls · 29 tokens"));
    assert!(
        html.contains("Model rounds")
            && html.contains("Tool calls")
            && html.contains("Token budget")
    );
    assert!(html.contains("Connect to the gateway to save limits"));
    assert!(html.contains("disabled"));
    let mut dom = VirtualDom::new_with_props(
        root,
        Shown {
            online: true,
            inherited: None,
        },
    );
    dom.rebuild_in_place();
    assert!(dioxus_ssr::render(&dom).contains("Inherited limits are unavailable"));
}
