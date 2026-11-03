//! POSIX shell walker over `tree-sitter-bash`.

use tree_sitter::{Node, Parser};

use super::env::Env;
use super::words::{self, Collector, Ctx, Found, Parts};
use super::{
    paths, Construct, Dialect, LoopKind, Origin, PipelineSlot, Redirect, UnknownReason, Value,
};

/// A `for x in ...` list longer than this is walked once with `x` unknown.
const MAX_UNROLL: usize = 16;

/// Bash's dynamic variables: set by the shell itself, different per read.
const DYNAMIC_VARIABLES: &[&str] = &[
    "RANDOM",
    "SRANDOM",
    "SECONDS",
    "LINENO",
    "BASHPID",
    "EPOCHSECONDS",
    "EPOCHREALTIME",
    "BASH_COMMAND",
    "FUNCNAME",
    "PIPESTATUS",
    "REPLY",
    "OLDPWD",
];

/// The text the grammar parses. Bash treats a carriage return as an
/// ordinary word character, but the grammar treats it as whitespace, so
/// `x\r#`cmd`` would read as a comment while bash runs `cmd`. The swap
/// keeps every byte offset, so all text is still sliced from the original.
fn grammar_input(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains('\r') {
        std::borrow::Cow::Owned(text.replace('\r', "\u{1}"))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

/// Parse `text` as a POSIX shell script and walk it into `out`.
pub(super) fn walk_text(text: &str, env: &mut Env, ctx: Ctx, out: &mut Collector) {
    let parsed = grammar_input(text);
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        out.has_error = true;
        return;
    }
    let Some(tree) = parser.parse(parsed.as_ref(), None) else {
        out.has_error = true;
        return;
    };
    let root = tree.root_node();
    if root.has_error() {
        out.syntax_error(words::first_syntax_error(root, text));
    }
    let mut walker = Walker {
        src: text,
        out,
        nesting: 0,
    };
    walker.walk(root, env, ctx);
}

struct Walker<'s, 'o> {
    src: &'s str,
    out: &'o mut Collector,
    nesting: usize,
}

impl<'s> Walker<'s, '_> {
    /// Source text of `node`, borrowed from the script rather than `self`.
    fn text(&self, node: Node<'_>) -> &'s str {
        let src: &'s str = self.src;
        node.utf8_text(src.as_bytes()).unwrap_or_default()
    }

    fn walk(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) {
        if self.nesting >= words::MAX_NESTING {
            self.out.depth_capped = true;
            return;
        }
        self.nesting += 1;
        self.walk_node(node, env, ctx);
        self.nesting -= 1;
    }

    fn walk_node(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) {
        match node.kind() {
            "program" | "do_group" | "list" | "else_clause" | "elif_clause" => {
                self.walk_sequence(node, env, ctx);
            }
            "compound_statement" => {
                self.out.construct(ctx, Construct::BraceGroup);
                self.walk_sequence(node, env, ctx);
            }
            "command" => self.command(node, env, ctx),
            "pipeline" => self.pipeline(node, env, ctx),
            "subshell" => self.subshell(node, env, ctx),
            "redirected_statement" => self.redirected(node, env, ctx),
            "variable_assignment" => self.assignment(node, env, ctx, None),
            "variable_assignments" => self.walk_sequence(node, env, ctx),
            "declaration_command" => self.declaration(node, env, ctx),
            "unset_command" => self.unset(node, env),
            "for_statement" => self.for_loop(node, env, ctx),
            "c_style_for_statement" => self.c_style_for(node, env, ctx),
            "while_statement" => self.while_loop(node, env, ctx),
            "if_statement" => self.if_statement(node, env, ctx),
            "case_statement" => self.case_statement(node, env, ctx),
            "function_definition" => self.function(node, env, ctx),
            "ERROR" => self.error(node, env, ctx),
            "comment" | "heredoc_body" | "heredoc_start" | "heredoc_end" => {}
            _ => self.scan_children(node, env, ctx),
        }
    }

    /// Walk named children in order, marking a child followed by `&` as a
    /// background job.
    fn walk_sequence(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) {
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        for (index, child) in children.iter().enumerate() {
            if !child.is_named() {
                continue;
            }
            let backgrounded = children
                .get(index + 1)
                .is_some_and(|next| next.kind() == "&");
            let child_ctx = if backgrounded {
                ctx.backgrounded()
            } else {
                ctx
            };
            self.walk(*child, env, child_ctx);
        }
    }

    /// Text the grammar could not parse. Commands inside it are still walked.
    /// After a heredoc that never ends, the grammar files the rest as loose
    /// words; the shell would read it as the body, but a delimiter typo is the
    /// likelier story, so fail toward scanning: read it as commands.
    fn error(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) {
        let ctx = ctx.errored();
        self.walk_sequence(node, env, ctx);
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        let unterminated = children.iter().any(|c| c.kind() == "heredoc_start")
            && !children.iter().any(|c| c.kind() == "heredoc_end");
        if !unterminated || ctx.depth >= words::MAX_DEPTH {
            return;
        }
        let start = children
            .iter()
            .find(|c| c.kind() == "heredoc_start")
            .map_or(node.start_byte(), |c| c.end_byte());
        let rest = &self.src[start..node.end_byte()];
        if let Some((_, after_line)) = rest.split_once('\n') {
            let mut inner = env.clone();
            walk_text(after_line, &mut inner, ctx, self.out);
        }
    }

    /// Unknown constructs: find commands and substitutions anywhere inside.
    fn scan_children(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "command_substitution" | "process_substitution" => {
                    self.substitution(child, env, ctx);
                }
                _ => self.walk(child, env, ctx),
            }
        }
    }

    fn pipeline(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) {
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        // A stray token the grammar could not place is not a stage.
        for stray in children.iter().filter(|child| child.kind() == "ERROR") {
            self.walk(*stray, &mut env.clone(), ctx);
        }
        let stages: Vec<Node<'_>> = children
            .into_iter()
            .filter(|child| !matches!(child.kind(), "comment" | "ERROR"))
            .collect();
        if stages.len() < 2 {
            for stage in stages {
                self.walk(stage, env, ctx);
            }
            return;
        }
        let id = self.out.pipeline_id();
        let count = stages.len();
        for (stage, child) in stages.into_iter().enumerate() {
            let slot = PipelineSlot {
                id,
                stage,
                stages: count,
            };
            // Each stage runs in its own subshell: its `cd` and
            // assignments do not reach the next command.
            let mut stage_env = env.clone();
            self.walk(child, &mut stage_env, ctx.in_pipeline(slot));
        }
    }

    fn subshell(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) {
        self.out.construct(ctx, Construct::Subshell);
        let mut inner = env.clone();
        self.walk_sequence(node, &mut inner, ctx.with_origin(Origin::Subshell));
    }

    fn redirected(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) {
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        let body = node.child_by_field_name("body");
        // `cat <<EOF | bash`: the grammar files the `| bash` pipeline under
        // the heredoc redirect. Rebuild the pipeline it really is.
        let piped = children
            .iter()
            .filter(|child| child.kind() == "heredoc_redirect")
            .find_map(|redirect| {
                let mut inner = redirect.walk();
                let found = redirect
                    .named_children(&mut inner)
                    .find(|child| child.kind() == "pipeline");
                found
            });
        let start = self.out.len();
        let feeds_shell = match (body, piped) {
            (Some(body), Some(pipe)) => self.heredoc_pipeline(body, pipe, env, ctx),
            (Some(body), None) => {
                let before = self.out.len();
                self.walk(body, env, ctx);
                self.out.len() > before && self.out.last().is_some_and(reads_script_from_stdin)
            }
            (None, _) => false,
        };
        let mut redirects = Vec::new();
        for child in &children {
            match child.kind() {
                "file_redirect" => redirects.extend(self.file_redirect(*child, env, ctx)),
                "heredoc_redirect" | "herestring_redirect" => {
                    redirects.push(self.input_redirect(*child));
                    // `cat <<'EOF' > run.sh`: the grammar files a redirect
                    // written after the heredoc marker under the heredoc.
                    let mut inner = child.walk();
                    let nested: Vec<Node<'_>> = child
                        .named_children(&mut inner)
                        .filter(|grandchild| grandchild.kind() == "file_redirect")
                        .collect();
                    for grandchild in nested {
                        redirects.extend(self.file_redirect(grandchild, env, ctx));
                    }
                }
                _ => {}
            }
        }
        if body.is_some() && piped.is_none() {
            self.out.redirect_since(start, ctx.depth, &redirects);
        }
        if body.is_some() {
            for heredoc in children
                .iter()
                .filter(|child| child.kind() == "heredoc_redirect")
            {
                let mut inner = heredoc.walk();
                let text = heredoc
                    .named_children(&mut inner)
                    .find(|child| child.kind() == "heredoc_body")
                    .map(|child| self.text(child));
                if let Some(text) = text {
                    self.out.heredoc_since(start, ctx.depth, text);
                }
            }
        }
        for child in children {
            if Some(child) != body {
                self.redirect(child, env, ctx, feeds_shell);
            }
        }
    }

    /// Walk `body | stage | ...` as one pipeline; whether any stage is a
    /// shell reading its script from stdin (and so runs the heredoc).
    fn heredoc_pipeline(
        &mut self,
        body: Node<'_>,
        pipe: Node<'_>,
        env: &mut Env,
        ctx: Ctx,
    ) -> bool {
        let mut cursor = pipe.walk();
        let mut stages = vec![body];
        stages.extend(
            pipe.named_children(&mut cursor)
                .filter(|child| child.kind() != "comment"),
        );
        let id = self.out.pipeline_id();
        let count = stages.len();
        let mut downstream_shell = false;
        for (stage, child) in stages.into_iter().enumerate() {
            let slot = PipelineSlot {
                id,
                stage,
                stages: count,
            };
            let before = self.out.len();
            let mut stage_env = env.clone();
            self.walk(child, &mut stage_env, ctx.in_pipeline(slot));
            downstream_shell |=
                self.out.len() > before && self.out.last().is_some_and(reads_script_from_stdin);
        }
        downstream_shell
    }

    /// A redirect's target may hold substitutions. A heredoc or here-string
    /// fed to a shell is code for that shell; an unquoted heredoc's body is
    /// data, but its `$(...)` and backticks still run.
    fn redirect(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx, feeds_shell: bool) {
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        let quoted = children
            .iter()
            .find(|child| child.kind() == "heredoc_start")
            .is_some_and(|start| self.text(*start).contains(['\'', '"', '\\']));
        for child in children {
            match child.kind() {
                "heredoc_body" if feeds_shell => self.nested_shell_text(self.text(child), env, ctx),
                "heredoc_body" if !quoted => self.heredoc_substitutions(self.text(child), env, ctx),
                "heredoc_body" | "heredoc_start" | "heredoc_end" | "pipeline" => {}
                _ if node.kind() == "file_redirect" => {}
                _ if node.kind() == "herestring_redirect" && feeds_shell => {
                    if let Value::Literal(script) = self.eval(child, env, ctx) {
                        self.nested_shell_text(&script, env, ctx);
                    }
                }
                _ => {
                    let _ = self.eval(child, env, ctx);
                }
            }
        }
    }

    /// `2> /dev/null`, `>&2`, `>> log`: the operator as written and the
    /// evaluated target.
    fn file_redirect(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) -> Option<Redirect> {
        let mut operator = String::new();
        let mut target = None;
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        for child in children {
            if child.kind() == "file_descriptor" || !child.is_named() {
                operator.push_str(self.text(child));
            } else if target.is_none() {
                target = Some(self.eval(child, env, ctx));
            }
        }
        Some(Redirect {
            operator,
            target: target?,
        })
    }

    /// `<<'EOF'`, `<<-EOF`, `<<< text`: the operator and what follows it, as
    /// written. Input redirections write nothing.
    fn input_redirect(&self, node: Node<'_>) -> Redirect {
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        let operator = children
            .iter()
            .find(|child| !child.is_named())
            .map_or("<<", |child| self.text(*child))
            .to_string();
        let target = children
            .iter()
            .find(|child| child.is_named() && child.kind() != "heredoc_body")
            .map_or_else(String::new, |child| self.text(*child).to_string());
        Redirect {
            operator,
            target: Value::literal(target),
        }
    }

    /// `text` is a script a child shell reads from stdin.
    fn nested_shell_text(&mut self, text: &str, env: &Env, ctx: Ctx) {
        if ctx.depth >= words::MAX_DEPTH {
            self.out.depth_capped = true;
            return;
        }
        let mut shell = env.child_shell(Dialect::Posix);
        walk_text(text, &mut shell, ctx.nested(Origin::NestedShell), self.out);
    }

    /// An unquoted heredoc body expands like a double-quoted string, so read
    /// it as one: the grammar parses `$(...)` in heredoc bodies but not
    /// backticks, and both run.
    fn heredoc_substitutions(&mut self, body: &str, env: &mut Env, ctx: Ctx) {
        let quoted = format!(": \"{}\"", body.replace('"', "\\\""));
        let mut parser = Parser::new();
        if parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
            .is_err()
        {
            return;
        }
        let Some(tree) = parser.parse(&quoted, None) else {
            return;
        };
        let mut walker = Walker {
            src: &quoted,
            out: &mut *self.out,
            nesting: self.nesting,
        };
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "string" {
                let _ = walker.eval(node, env, ctx);
                continue;
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
    }

    fn command(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) {
        let mut assignments = Vec::new();
        let mut words = Vec::new();
        let mut raw = Vec::new();
        let mut redirects = Vec::new();
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        for child in children {
            let word_node = match child.kind() {
                "variable_assignment" => {
                    if let Some((name, value)) = self.assignment_value(child, env, ctx) {
                        assignments.push((name, value));
                    }
                    continue;
                }
                "command_name" => match child.named_child(0) {
                    Some(inner) => inner,
                    None => continue,
                },
                "file_redirect" => {
                    redirects.extend(self.file_redirect(child, env, ctx));
                    continue;
                }
                "heredoc_redirect" | "herestring_redirect" | "comment" => {
                    let _ = self.eval(child, env, ctx);
                    continue;
                }
                _ => child,
            };
            let values = self.eval_split(word_node, env, ctx);
            let text = self.text(word_node).to_string();
            raw.extend(std::iter::repeat_n(text, values.len()));
            words.extend(values);
        }
        let source = self.text(node);
        let found = Found {
            words: words.clone(),
            raw,
            assignments,
            redirects,
            source,
        };
        if self.out.emit(found, env, ctx) {
            self.builtin_effects(&words, env, ctx);
        }
    }

    /// Shell-state effects of the command that just ran.
    fn builtin_effects(&mut self, words: &[Value], env: &mut Env, ctx: Ctx) {
        let Some(program) = words
            .first()
            .and_then(Value::as_literal)
            .map(paths::program_name)
        else {
            return;
        };
        let args = &words[1..];
        match program.as_str() {
            "read" => read_builtin(args, env),
            "export" | "declare" | "typeset" | "local" | "readonly" => {}
            "unset" | "unsetenv" => {
                for name in args.iter().filter_map(Value::as_literal) {
                    self.out.assigned(name);
                    env.unset(name);
                }
            }
            "setenv" => {
                if let Some(name) = args.first().and_then(Value::as_literal) {
                    self.out.assigned(name);
                    let value = args.get(1).cloned().unwrap_or_else(|| Value::literal(""));
                    env.set(name, value, Some(true));
                }
            }
            _ => {
                if !words::apply_directory_builtin(Dialect::Posix, &program, args, env) {
                    words::walk_nested(words, env, ctx, self.out);
                }
            }
        }
    }

    /// `NAME=value` as a statement: assigns in the current shell.
    fn assignment(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx, exported: Option<bool>) {
        if let Some(name_node) = node.child_by_field_name("name") {
            self.out.assigned(self.text(name_node));
        }
        if let Some((name, value)) = self.assignment_value(node, env, ctx) {
            env.set(&name, value, exported);
        }
    }

    fn assignment_value(
        &mut self,
        node: Node<'_>,
        env: &mut Env,
        ctx: Ctx,
    ) -> Option<(String, Value)> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.text(name_node).to_string();
        if name_node.kind() != "variable_name" {
            // `arr[i]=x`: an array element; leave the variable unknown.
            let base = name.split('[').next().unwrap_or(&name).to_string();
            env.set_runtime(&base);
            return None;
        }
        let value = match node.child_by_field_name("value") {
            Some(value) => self.eval(value, env, ctx),
            None => Value::literal(""),
        };
        let appends = node
            .children(&mut node.walk())
            .any(|child| child.kind() == "+=");
        if appends {
            return Some((
                name.clone(),
                Value::unknown(format!("${name}"), UnknownReason::RuntimeValue),
            ));
        }
        Some((name, value))
    }

    fn declaration(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) {
        let keyword = node
            .child(0)
            .map(|child| self.text(child).to_string())
            .unwrap_or_default();
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        let exports = keyword == "export"
            || children.iter().any(|child| {
                child.kind() == "word"
                    && self.text(*child).contains('x')
                    && self.text(*child).starts_with('-')
            });
        let flag = exports.then_some(true);
        for child in children {
            match child.kind() {
                "variable_assignment" => self.assignment(child, env, ctx, flag),
                "variable_name" => {
                    let name = self.text(child);
                    self.out.assigned(name);
                    if exports {
                        env.export(name);
                    }
                }
                _ => {
                    let _ = self.eval(child, env, ctx);
                }
            }
        }
    }

    fn unset(&mut self, node: Node<'_>, env: &mut Env) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if matches!(child.kind(), "variable_name" | "word") {
                let name = self.text(child);
                self.out.assigned(name);
                env.unset(name);
            }
        }
    }

    fn for_loop(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) {
        let select = node
            .child(0)
            .is_some_and(|keyword| keyword.kind() == "select");
        let (keyword, kind) = if select {
            ("select", LoopKind::While)
        } else {
            ("for", LoopKind::ForEach)
        };
        self.out.construct(ctx, Construct::ControlFlow(keyword));
        self.out.loop_seen(kind);
        let Some(variable) = node.child_by_field_name("variable") else {
            return;
        };
        let name = self.text(variable).to_string();
        let mut cursor = node.walk();
        let value_nodes: Vec<Node<'_>> =
            node.children_by_field_name("value", &mut cursor).collect();
        let mut items = Vec::new();
        for value in &value_nodes {
            items.extend(self.eval_split(*value, env, ctx));
        }
        let body = node.child_by_field_name("body");
        let loop_ctx = ctx.in_loop(kind);
        let known = !select
            && !value_nodes.is_empty()
            && !items.is_empty()
            && items.len() <= MAX_UNROLL
            && items.iter().all(|item| !item.is_unknown());
        if known {
            for item in items {
                env.set(&name, item, None);
                if let Some(body) = body {
                    self.walk(body, env, loop_ctx);
                }
            }
            return;
        }
        let before = env.clone();
        env.set_runtime(&name);
        if let Some(body) = body {
            self.walk(body, env, loop_ctx);
        }
        let after = env.clone();
        env.join(&[before, after]);
    }

    fn c_style_for(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) {
        let infinite = node.child_by_field_name("condition").is_none();
        let kind = LoopKind::CStyle { infinite };
        self.out.construct(ctx, Construct::ControlFlow("for"));
        self.out.loop_seen(kind);
        let loop_ctx = ctx.in_loop(kind);
        let before = env.clone();
        self.scan_children(node, env, loop_ctx);
        let after = env.clone();
        env.join(&[before, after]);
    }

    fn while_loop(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) {
        let until = node
            .child(0)
            .is_some_and(|keyword| keyword.kind() == "until");
        let kind = if until {
            LoopKind::Until
        } else if reads_lines(node, self.src) {
            // `... | while read x; do` ends at EOF: it walks lines (#2489).
            LoopKind::ForEach
        } else {
            LoopKind::While
        };
        let keyword = if until { "until" } else { "while" };
        self.out.construct(ctx, Construct::ControlFlow(keyword));
        self.out.loop_seen(kind);
        let before = env.clone();
        self.walk_sequence(node, env, ctx.in_loop(kind));
        let after = env.clone();
        env.join(&[before, after]);
    }

    /// Conditions run in order; each branch starts from the state after
    /// the conditions, and the branches are joined afterwards.
    fn if_statement(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) {
        self.out.construct(ctx, Construct::ControlFlow("if"));
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        let then_at = children.iter().position(|child| child.kind() == "then");
        let Some(then_at) = then_at else {
            self.scan_children(node, env, ctx);
            return;
        };
        for condition in children[..then_at].iter().filter(|child| child.is_named()) {
            self.walk(*condition, env, ctx);
        }
        let start = env.clone();
        let mut outcomes = Vec::new();
        let mut branch = start.clone();
        let mut has_else = false;
        for child in &children[then_at + 1..] {
            match child.kind() {
                "elif_clause" | "else_clause" => {
                    outcomes.push(std::mem::replace(&mut branch, start.clone()));
                    has_else |= child.kind() == "else_clause";
                    self.walk(*child, &mut branch, ctx);
                }
                _ if child.is_named() => self.walk(*child, &mut branch, ctx),
                _ => {}
            }
        }
        outcomes.push(branch);
        if !has_else {
            outcomes.push(start);
        }
        env.join(&outcomes);
    }

    fn case_statement(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) {
        self.out.construct(ctx, Construct::ControlFlow("case"));
        let start = env.clone();
        let mut outcomes = vec![start.clone()];
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "case_item" {
                let mut branch = start.clone();
                self.scan_children(child, &mut branch, ctx);
                outcomes.push(branch);
            } else {
                let _ = self.eval(child, env, ctx);
            }
        }
        env.join(&outcomes);
    }

    /// A function body runs only when called: walk it once, with unknown
    /// positional parameters, without touching the caller's state.
    fn function(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) {
        self.out.construct(ctx, Construct::FunctionDefinition);
        if let Some(body) = node.child_by_field_name("body") {
            let mut inner = env.clone();
            // The body's braces are the function's, not a brace group.
            let body_ctx = ctx.with_origin(Origin::FunctionBody);
            if body.kind() == "compound_statement" {
                self.walk_sequence(body, &mut inner, body_ctx);
            } else {
                self.walk(body, &mut inner, body_ctx);
            }
        }
    }

    fn substitution(&mut self, node: Node<'_>, env: &Env, ctx: Ctx) -> Value {
        let construct = if node.kind() == "process_substitution" {
            Construct::ProcessSubstitution
        } else if self.text(node).starts_with('`') {
            Construct::BacktickSubstitution
        } else {
            Construct::CommandSubstitution
        };
        self.out.construct(ctx, construct);
        if ctx.depth < words::MAX_DEPTH {
            let mut inner = env.clone();
            self.walk_sequence(node, &mut inner, ctx.nested(Origin::Substitution));
        } else {
            self.out.depth_capped = true;
        }
        Value::unknown(self.text(node), UnknownReason::CommandSubstitution)
    }

    /// Evaluate a word, splitting an unquoted expansion on whitespace the
    /// way the shell does (`$FLAGS` with `-r -f` is two words; an empty
    /// unquoted `$EMPTY` is none).
    fn eval_split(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) -> Vec<Value> {
        let value = self.eval(node, env, ctx);
        let splits = matches!(node.kind(), "simple_expansion" | "expansion");
        match value {
            Value::Literal(text) if splits => text.split_whitespace().map(Value::literal).collect(),
            other => vec![other],
        }
    }

    fn eval(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) -> Value {
        match node.kind() {
            "word" => self.word(node, env),
            "number" => Value::literal(self.text(node)),
            "raw_string" => Value::literal(strip_quotes(self.text(node))),
            "ansi_c_string" => Value::literal(ansi_c(self.text(node))),
            "string" | "translated_string" => self.string(node, env, ctx),
            "concatenation" => self.concatenation(node, env, ctx),
            "simple_expansion" => self.simple_expansion(node, env),
            "expansion" => self.expansion(node, env, ctx),
            "command_substitution" | "process_substitution" => self.substitution(node, env, ctx),
            "brace_expression" => Value::literal(self.text(node)),
            _ => {
                self.scan_children(node, env, ctx);
                Value::unknown(self.text(node), UnknownReason::Unsupported)
            }
        }
    }

    fn word(&self, node: Node<'_>, env: &Env) -> Value {
        let text = self.text(node);
        if text.starts_with('~') {
            if let Some(expanded) = paths::expand_tilde(text, env.home().as_deref()) {
                return Value::literal(expanded);
            }
        }
        Value::literal(unescape_bare(text))
    }

    fn string(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) -> Value {
        let mut parts = Parts::default();
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        let mut last_end = None;
        for child in children {
            // Text between children (whitespace, mostly) is not its own node.
            if let Some(end) = last_end {
                parts.push_text(&self.src[end..child.start_byte().max(end)]);
            }
            last_end = Some(child.end_byte());
            if child.kind() == "\"" || child.kind() == "$\"" {
                continue;
            }
            if child.kind() == "string_content" {
                parts.push_text(&unescape_double_quoted(self.text(child)));
            } else {
                let value = self.eval(child, env, ctx);
                parts.push(&value);
            }
        }
        parts.finish()
    }

    fn concatenation(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) -> Value {
        let mut parts = Parts::default();
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        for (index, child) in children.iter().enumerate() {
            let value = if index == 0 && child.kind() == "word" {
                self.word(*child, env)
            } else if child.kind() == "word" {
                Value::literal(unescape_bare(self.text(*child)))
            } else {
                self.eval(*child, env, ctx)
            };
            parts.push(&value);
        }
        parts.finish()
    }

    fn simple_expansion(&self, node: Node<'_>, env: &Env) -> Value {
        let raw = self.text(node);
        let Some(name_node) = node.named_child(0) else {
            return Value::literal(raw);
        };
        let name = self.text(name_node);
        if name_node.kind() == "special_variable_name" || name.chars().all(|ch| ch.is_ascii_digit())
        {
            return Value::unknown(raw, UnknownReason::SpecialParameter);
        }
        lookup(name, raw, env)
    }

    fn expansion(&mut self, node: Node<'_>, env: &mut Env, ctx: Ctx) -> Value {
        let raw = self.text(node).to_string();
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node
            .children(&mut cursor)
            .filter(|child| !matches!(child.kind(), "${" | "}"))
            .collect();
        let Some(name_at) = children
            .iter()
            .position(|child| matches!(child.kind(), "variable_name" | "special_variable_name"))
        else {
            return Value::unknown(raw, UnknownReason::Unsupported);
        };
        if name_at > 0 {
            // `${#X}`, `${!X}`: length and indirection.
            return Value::unknown(raw, UnknownReason::Unsupported);
        }
        let name_node = children[name_at];
        let name = self.text(name_node).to_string();
        if name_node.kind() == "special_variable_name" || name.chars().all(|ch| ch.is_ascii_digit())
        {
            return Value::unknown(raw, UnknownReason::SpecialParameter);
        }
        let current = lookup(&name, &raw, env);
        let Some(operator) = children.get(name_at + 1) else {
            return current;
        };
        let operand = &children[name_at + 2..];
        self.expansion_operator(
            self.text(*operator).to_string(),
            &name,
            current,
            operand,
            env,
            ctx,
            raw,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn expansion_operator(
        &mut self,
        operator: String,
        name: &str,
        current: Value,
        operand: &[Node<'_>],
        env: &mut Env,
        ctx: Ctx,
        raw: String,
    ) -> Value {
        let set = current.as_literal().is_some();
        let non_empty = current.as_literal().is_some_and(|text| !text.is_empty());
        let use_default = match operator.as_str() {
            ":-" | ":=" => !non_empty,
            "-" | "=" => !set,
            ":+" => {
                return if non_empty {
                    self.operand(operand, env, ctx)
                } else {
                    Value::literal("")
                }
            }
            "+" => {
                return if set {
                    self.operand(operand, env, ctx)
                } else {
                    Value::literal("")
                }
            }
            _ => return Value::unknown(raw, UnknownReason::Unsupported),
        };
        if current.is_unknown()
            && !matches!(current, Value::Unknown(ref u) if u.reason == UnknownReason::UnsetVariable)
        {
            return Value::unknown(raw, UnknownReason::RuntimeValue);
        }
        if !use_default {
            return current;
        }
        let value = self.operand(operand, env, ctx);
        if operator.ends_with('=') {
            env.set(name, value.clone(), None);
        }
        value
    }

    fn operand(&mut self, nodes: &[Node<'_>], env: &mut Env, ctx: Ctx) -> Value {
        let mut parts = Parts::default();
        for (index, node) in nodes.iter().enumerate() {
            let value = if index == 0 && node.kind() == "word" {
                self.word(*node, env)
            } else {
                self.eval(*node, env, ctx)
            };
            parts.push(&value);
        }
        parts.finish()
    }
}

/// A POSIX shell with no `-c` script and no script file: it runs whatever
/// arrives on stdin.
fn reads_script_from_stdin(invocation: &super::Invocation) -> bool {
    let argv = invocation.effective_argv();
    let is_shell = argv
        .first()
        .and_then(Value::as_literal)
        .is_some_and(|program| words::is_posix_shell(&paths::program_name(program)));
    is_shell
        && argv[1..].iter().all(|word| {
            word.as_literal().is_some_and(|text| {
                text.starts_with('-') && !text.trim_start_matches('-').contains('c')
            }) || word.as_literal() == Some("-s")
        })
}

/// Whether a `while` loop's condition is a bare `read`, which fails at EOF.
fn reads_lines(node: Node<'_>, src: &str) -> bool {
    node.child_by_field_name("condition")
        .and_then(|condition| condition.utf8_text(src.as_bytes()).ok())
        .and_then(|text| text.trim().trim_end_matches(';').split_whitespace().next())
        == Some("read")
}

fn lookup(name: &str, raw: &str, env: &Env) -> Value {
    if DYNAMIC_VARIABLES.contains(&name) && env.get(name).is_none() {
        return Value::unknown(raw, UnknownReason::SpecialParameter);
    }
    match env.get(name) {
        Some(Value::Literal(text)) => Value::literal(text.clone()),
        Some(Value::Unknown(unknown)) => Value::unknown(raw, unknown.reason),
        None if DYNAMIC_VARIABLES.contains(&name) => {
            Value::unknown(raw, UnknownReason::SpecialParameter)
        }
        None => Value::unknown(raw, UnknownReason::UnsetVariable),
    }
}

/// `read [-options] [NAME...]`: every named variable becomes input.
fn read_builtin(args: &[Value], env: &mut Env) {
    const WITH_VALUE: &[&str] = &["-p", "-t", "-u", "-d", "-n", "-N", "-a", "-i"];
    let mut names = Vec::new();
    let mut index = 0;
    while let Some(word) = args.get(index) {
        let text = word.text();
        if WITH_VALUE.contains(&text) {
            if text == "-a" {
                if let Some(name) = args.get(index + 1) {
                    names.push(name.text().to_string());
                }
            }
            index += 2;
        } else {
            if !text.starts_with('-') {
                names.push(text.to_string());
            }
            index += 1;
        }
    }
    if names.is_empty() {
        names.push("REPLY".to_string());
    }
    for name in names {
        env.set_runtime(&name);
    }
}

fn strip_quotes(text: &str) -> &str {
    let text = text.strip_prefix('$').unwrap_or(text);
    text.get(1..text.len().saturating_sub(1))
        .unwrap_or_default()
}

/// An unquoted word: a backslash makes the next character literal.
///
/// One deliberate deviation from bash: a backslash before a letter or digit
/// is kept. Bash would drop it (`C:\tools\x` becomes `C:toolsx`), but that
/// spelling only ever means a Windows path typed into Git Bash, and the
/// guards must see the program it names.
fn unescape_bare(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.peek().copied() {
            Some(next) if next.is_ascii_alphanumeric() => out.push('\\'),
            Some('\n') => {
                chars.next();
            }
            Some(next) => {
                out.push(next);
                chars.next();
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Inside double quotes a backslash only escapes `$`, `` ` ``, `"`, `\`
/// and a newline.
fn unescape_double_quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.peek() {
                Some('$' | '`' | '"' | '\\') => {
                    out.extend(chars.next());
                }
                Some('\n') => {
                    chars.next();
                }
                _ => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// `$'...'`: decode the common C escapes.
fn ansi_c(text: &str) -> String {
    let body = strip_quotes(text);
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('a') => out.push('\u{7}'),
            Some('b') => out.push('\u{8}'),
            Some('e' | 'E') => out.push('\u{1b}'),
            Some('v') => out.push('\u{b}'),
            Some('f') => out.push('\u{c}'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}
