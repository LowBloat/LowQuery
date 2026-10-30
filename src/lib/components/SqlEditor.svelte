<script lang="ts">
	import { onMount } from "svelte";
	import { EditorState, Compartment, StateEffect, StateField, type Extension } from "@codemirror/state";
	import {
		EditorView,
		Decoration,
		keymap,
		lineNumbers,
		highlightActiveLine,
		highlightActiveLineGutter,
		drawSelection,
		highlightSpecialChars,
		placeholder as cmPlaceholder,
		type DecorationSet,
	} from "@codemirror/view";
	import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
	import {
		bracketMatching,
		indentOnInput,
		syntaxHighlighting,
		HighlightStyle,
	} from "@codemirror/language";
	import {
		autocompletion,
		closeBrackets,
		closeBracketsKeymap,
		completionKeymap,
	} from "@codemirror/autocomplete";
	import { highlightSelectionMatches, searchKeymap, search } from "@codemirror/search";
	import { sql, PostgreSQL, PLSQL, MySQL, MariaSQL, MSSQL, SQLite, Cassandra, type SQLDialect, type SQLNamespace } from "@codemirror/lang-sql";
	import { tags as t } from "@lezer/highlight";
	import type { Engine } from "../api";
	import { redisSupport } from "../engines/redis-language";
	import type { EditorError } from "../state/workspace.svelte";

	interface Props {
		value: string;
		engine: Engine;
		schema?: SQLNamespace;
		defaultSchema?: string;
		error?: EditorError | null;
		label?: string;
		onchange?: (value: string) => void;
		oncursor?: (pos: number, selection: string, from: number) => void;
		onrun?: (mode: "statement" | "script") => void;
	}

	let {
		value,
		engine,
		schema = {},
		defaultSchema,
		error = null,
		label = "SQL editor",
		onchange,
		oncursor,
		onrun,
	}: Props = $props();

	let host: HTMLDivElement;
	let view: EditorView | undefined;
	const language = new Compartment();

	const setError = StateEffect.define<EditorError | null>();
	const errorField = StateField.define<DecorationSet>({
		create: () => Decoration.none,
		update(deco, tr) {
			for (const e of tr.effects) {
				if (e.is(setError)) {
					if (!e.value) return Decoration.none;
					const len = tr.state.doc.length;
					const from = Math.min(e.value.from, len);
					const to = Math.min(Math.max(e.value.to, from + 1), len);
					if (to <= from) return Decoration.none;
					return Decoration.set([
						Decoration.mark({ class: "lq-cm-error", attributes: { title: e.value.message } }).range(from, to),
					]);
				}
			}
			// Any edit clears the highlight.
			return tr.docChanged ? Decoration.none : deco;
		},
		provide: (f) => EditorView.decorations.from(f),
	});

	const highlight = HighlightStyle.define([
		{ tag: [t.keyword, t.operatorKeyword, t.modifier], color: "var(--lq-syn-keyword)", fontWeight: "600" },
		{ tag: [t.string, t.special(t.string)], color: "var(--lq-syn-string)" },
		{ tag: [t.number, t.bool, t.null], color: "var(--lq-syn-number)" },
		{ tag: [t.lineComment, t.blockComment, t.comment], color: "var(--lq-syn-comment)", fontStyle: "italic" },
		{ tag: [t.typeName, t.standard(t.name)], color: "var(--lq-syn-type)" },
		{ tag: [t.special(t.name), t.variableName], color: "var(--lq-syn-name)" },
		{ tag: [t.operator, t.punctuation, t.bracket], color: "var(--lq-syn-punct)" },
	]);

	const editorTheme = EditorView.theme({
		"&": { height: "100%", fontSize: "14px", backgroundColor: "var(--ca-surface-raised)", color: "var(--ca-text-primary)" },
		".cm-scroller": { fontFamily: "var(--ca-font-mono)", lineHeight: "1.55" },
		".cm-content": { caretColor: "var(--ca-text-primary)", padding: "8px 0" },
		".cm-gutters": {
			backgroundColor: "var(--ca-surface-sunken)",
			color: "var(--ca-text-hint)",
			border: "none",
			borderRight: "1px solid var(--ca-outline-subtle)",
		},
		".cm-activeLine": { backgroundColor: "var(--lq-active-line)" },
		".cm-activeLineGutter": { backgroundColor: "var(--lq-active-line)", color: "var(--ca-text-primary)" },
		"&.cm-focused .cm-cursor": { borderLeftColor: "var(--ca-text-primary)" },
		"&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection": {
			backgroundColor: "var(--lq-selection) !important",
		},
		".cm-matchingBracket": { backgroundColor: "var(--lq-bracket)", outline: "none" },
		".cm-selectionMatch": { backgroundColor: "var(--lq-match)" },
		".cm-tooltip": {
			border: "1px solid var(--ca-outline-default)",
			backgroundColor: "var(--ca-surface-overlay)",
			color: "var(--ca-text-primary)",
		},
		".cm-tooltip-autocomplete > ul > li[aria-selected]": {
			backgroundColor: "var(--ca-brand-fill)",
			color: "var(--ca-brand-contrast)",
		},
		".cm-panels": { backgroundColor: "var(--ca-surface-sunken)", color: "var(--ca-text-primary)" },
		".cm-placeholder": { color: "var(--ca-text-hint)" },
	});

	// Keyword sets, quoting and comment rules of each engine for highlighting
	// and completion.
	const DIALECTS: Record<Engine, SQLDialect> = {
		postgres: PostgreSQL,
		oracle: PLSQL,
		mysql: MySQL,
		mariadb: MariaSQL,
		sqlserver: MSSQL,
		sqlite: SQLite,
		redis: PostgreSQL,
		cassandra: Cassandra,
	};

	function languageFor(e: Engine, ns: SQLNamespace, def?: string): Extension {
		if (e === "redis") {
			// Keys loaded in the tree, from every database.
			const keys = Object.values(ns as Record<string, Record<string, unknown>>).flatMap((db) => Object.keys(db ?? {}));
			return redisSupport(keys);
		}
		return sql({
			dialect: DIALECTS[e] ?? PostgreSQL,
			schema: ns,
			defaultSchema: def,
			upperCaseKeywords: false,
		});
	}

	function emitCursor(v: EditorView) {
		const sel = v.state.selection.main;
		oncursor?.(sel.head, v.state.sliceDoc(sel.from, sel.to), sel.from);
	}

	onMount(() => {
		const runKeys = keymap.of([
			{ key: "Mod-Enter", preventDefault: true, run: () => (onrun?.("statement"), true) },
			{ key: "Shift-Mod-Enter", preventDefault: true, run: () => (onrun?.("script"), true) },
			{ key: "F5", preventDefault: true, run: () => (onrun?.("script"), true) },
		]);
		view = new EditorView({
			parent: host,
			state: EditorState.create({
				doc: value,
				extensions: [
					runKeys,
					lineNumbers(),
					highlightActiveLineGutter(),
					highlightSpecialChars(),
					history(),
					drawSelection(),
					indentOnInput(),
					bracketMatching(),
					closeBrackets(),
					autocompletion({ activateOnTyping: true }),
					highlightActiveLine(),
					highlightSelectionMatches(),
					search({ top: true }),
					keymap.of([
						...closeBracketsKeymap,
						...defaultKeymap,
						...searchKeymap,
						...historyKeymap,
						...completionKeymap,
						indentWithTab,
					]),
					language.of(languageFor(engine, schema, defaultSchema)),
					syntaxHighlighting(highlight),
					editorTheme,
					errorField,
					cmPlaceholder("Write SQL here. Ctrl+Enter runs the statement under the cursor; Ctrl+Shift+Enter runs the script."),
					EditorView.contentAttributes.of({ "aria-label": label }),
					EditorView.updateListener.of((u) => {
						if (u.docChanged) onchange?.(u.state.doc.toString());
						if (u.docChanged || u.selectionSet) emitCursor(u.view);
					}),
				],
			}),
		});
		return () => view?.destroy();
	});

	// Content replaced from outside (different tab, insert from the schema browser).
	$effect(() => {
		const v = value;
		if (view && v !== view.state.doc.toString()) {
			view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: v } });
		}
	});

	$effect(() => {
		const ext = languageFor(engine, schema, defaultSchema);
		view?.dispatch({ effects: language.reconfigure(ext) });
	});

	$effect(() => {
		const e = error;
		if (!view) return;
		view.dispatch({
			effects: setError.of(e),
			...(e ? { selection: { anchor: Math.min(e.from, view.state.doc.length) }, scrollIntoView: true } : {}),
		});
	});

	export function focus() {
		view?.focus();
	}

	/** Inserts text at the cursor position (used by the schema browser). */
	export function insert(text: string) {
		if (!view) return;
		const sel = view.state.selection.main;
		view.dispatch({
			changes: { from: sel.from, to: sel.to, insert: text },
			selection: { anchor: sel.from + text.length },
			scrollIntoView: true,
		});
		view.focus();
	}
</script>

<div class="lq-editor" bind:this={host}></div>

<style>
	.lq-editor {
		height: 100%;
		min-height: 0;
		overflow: hidden;
	}
	.lq-editor :global(.cm-editor) {
		height: 100%;
	}
	.lq-editor :global(.cm-editor.cm-focused) {
		outline: none;
	}
	.lq-editor :global(.lq-cm-error) {
		text-decoration: underline wavy var(--ca-semantic-error);
		text-underline-offset: 3px;
		background: color-mix(in oklab, var(--ca-semantic-error) 14%, transparent);
	}
</style>
