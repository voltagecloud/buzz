// Builders for the element trees Buzz draws (`docs/plugins.md`, "Elements").
// Each returns a plain object; children that are `null`, `undefined` or
// `false` are left out, so `cond && text("…")` works in a list.

export type Gap = "none" | "sm" | "md" | "lg";
export type Tone = "default" | "subtle" | "accent" | "success" | "warning" | "danger" | "complete" | "progress";
export type Justify = "start" | "center" | "end" | "between";
export type Align = "start" | "center" | "end" | "stretch";
export type TextStyle = "body" | "small" | "large" | "label" | "title" | "heading" | "code";
export type ButtonVariant = "prominent" | "subtle" | "ghost" | "outline" | "destructive";
export type Background = "inset" | "panel";

export interface Element {
  type: string;
  [field: string]: unknown;
}

// Falsy values, as `cond && el.text(…)` gives them, are left out.
export type Child = Element | null | undefined | false | "" | 0;

function kids(children: Child[]): Element[] {
  return children.filter((child): child is Element => Boolean(child));
}

/** Drops fields left `undefined`, so trees stay small on the wire. */
function element(type: string, fields: Record<string, unknown>): Element {
  const out: Element = { type };
  for (const [key, value] of Object.entries(fields)) {
    if (value !== undefined) out[key] = value;
  }
  return out;
}

export interface ColumnProps {
  gap?: Gap;
  align?: Align;
  justify?: Justify;
  grow?: boolean;
  padding?: Gap;
  background?: Background;
}

export function column(children: Child[], props: ColumnProps = {}): Element {
  return element("column", { ...props, children: kids(children) });
}

export interface RowProps {
  gap?: Gap;
  justify?: Justify;
  wrap?: boolean;
  grow?: boolean;
  padding?: Gap;
  background?: Background;
}

export function row(children: Child[], props: RowProps = {}): Element {
  return element("row", { ...props, children: kids(children) });
}

export function card(children: Child[], props: { title?: string; padding?: Gap; gap?: Gap } = {}): Element {
  return element("card", { ...props, children: kids(children) });
}

/** A page or panel's title bar; `actions` sit at its end. */
export function header(title: string, props: { icon?: string; subtitle?: string; actions?: Child[] } = {}): Element {
  return element("header", { title, icon: props.icon, subtitle: props.subtitle, children: kids(props.actions ?? []) });
}

export function text(
  value: string,
  props: { style?: TextStyle; tone?: Tone; truncate?: boolean; tooltip?: string } = {},
): Element {
  return element("text", { text: value, ...props });
}

export function markdown(value: string): Element {
  return element("markdown", { text: value });
}

export interface ButtonProps {
  variant?: ButtonVariant;
  icon?: string;
  disabled?: boolean;
  loading?: boolean;
  /** `sm` for a small one. With an empty label and an icon, an icon button. */
  size?: "sm";
  tooltip?: string;
  /** Takes the full width of its column. */
  fill?: boolean;
}

export function button(id: string, label: string, props: ButtonProps = {}): Element {
  return element("button", { id, label, ...props });
}

/** Opens like a link in a message, or with `external`, always in the browser. */
export function link(label: string, url: string, props: { external?: boolean } = {}): Element {
  return element("link", { label, url, ...props });
}

export interface InputProps {
  value?: string;
  placeholder?: string;
  /** Raise it to put `value` into the field again. */
  version?: number;
  clearOnSubmit?: boolean;
  disabled?: boolean;
  /** Takes the width left in its row. */
  grow?: boolean;
}

export function input(id: string, props: InputProps = {}): Element {
  return element("input", { id, ...props });
}

/** A text area of several lines; it sends `change` as the text changes. */
export function textarea(
  id: string,
  props: { value?: string; placeholder?: string; version?: number; rows?: number; maxRows?: number; disabled?: boolean } = {},
): Element {
  return element("textarea", { id, ...props });
}

export function toggle(id: string, checked: boolean, props: { label?: string } = {}): Element {
  return element("switch", { id, checked, ...props });
}

export function checkbox(
  id: string,
  checked: boolean,
  props: { label?: string; indeterminate?: boolean; completed?: boolean; disabled?: boolean } = {},
): Element {
  return element("checkbox", { id, checked, ...props });
}

export function choice(id: string, options: { value: string; label: string }[], value?: string): Element {
  return element("choice", { id, options, value });
}

export function progress(value: number): Element {
  return element("progress", { value });
}

export function image(url: string, props: { width?: number; height?: number; alt?: string } = {}): Element {
  return element("image", { url, ...props });
}

/** Someone's picture by `pubkey`, or with `url`, a picture from the web with `name`'s initial. */
export function avatar(
  pubkey: string,
  props: { size?: "small" | "default" | "large"; url?: string; name?: string } = {},
): Element {
  return element("avatar", { pubkey, ...props });
}

export function badge(value: string, props: { tone?: Tone } = {}): Element {
  return element("badge", { text: value, ...props });
}

export function icon(name: string, props: { size?: number; tone?: Tone } = {}): Element {
  return element("icon", { name, ...props });
}

export interface MenuOption {
  value: string;
  label: string;
  icon?: string;
  iconTone?: Tone;
  /** A person's picture before the label. */
  avatar?: string;
  disabled?: boolean;
}

export interface MenuProps {
  /** A button with this label opens it, in place of a small icon button. */
  label?: string;
  icon?: string;
  iconTone?: Tone;
  /** Shows this person's picture as the button. */
  avatar?: string;
  tooltip?: string;
  title?: string;
  value?: string;
  disabled?: boolean;
}

/** A small button opening a menu; choosing sends `change` with the option's value. */
export function menu(id: string, options: MenuOption[], props: MenuProps = {}): Element {
  return element("menu", { id, options, ...props });
}

/** A field that opens a list of options; choosing sends `change` with the option's value. */
export function select(
  id: string,
  options: MenuOption[],
  props: { value?: string; placeholder?: string; disabled?: boolean; width?: number } = {},
): Element {
  return element("select", { id, options, ...props });
}

export interface CollapsibleProps {
  open?: boolean;
  marker?: Child[];
  markerTooltip?: string;
  header?: Child[];
  /** One line shown while closed. */
  preview?: string;
  /** When this text fits the header on one line, it does not open and shows `children` in place of the preview. */
  fit?: string;
  expandable?: boolean;
  below?: Child[];
  actions?: Child[];
  footer?: Child[];
}

/** A message that opens to show `children`; clicking it sends `toggle`. */
export function collapsible(id: string, children: Child[], props: CollapsibleProps = {}): Element {
  return element("collapsible", {
    id,
    open: props.open,
    marker: props.marker && kids(props.marker),
    markerTooltip: props.markerTooltip,
    header: props.header && kids(props.header),
    preview: props.preview,
    fit: props.fit,
    expandable: props.expandable,
    children: kids(children),
    below: props.below && kids(props.below),
    actions: props.actions && kids(props.actions),
    footer: props.footer && kids(props.footer),
  });
}

/** GitHub-flavoured Markdown, as GitHub shows a body; `html` is what GitHub rendered for it, `base` what links resolve against. */
export function document(value: string, props: { html?: string; base?: string } = {}): Element {
  return element("document", { text: value, ...props });
}

/** As many square thumbnails as fit, the last counting the rest. */
export function thumbnails(images: string[], props: { label?: string } = {}): Element {
  return element("thumbnails", { images, ...props });
}

export function tabs(id: string, options: { value: string; label: string }[], value?: string): Element {
  return element("tabs", { id, options, value });
}

/** Coloured arcs, one per segment. */
export function ring(segments: { value: number; tone?: Tone }[], props: { size?: number } = {}): Element {
  return element("ring", { segments, ...props });
}

/** A small button that shows a note under it. */
export function popover(
  id: string,
  props: { icon?: string; tooltip?: string; title?: string; description?: string },
): Element {
  return element("popover", { id, ...props });
}

/** An empty list's placeholder. */
export function empty(title: string, props: { icon?: string; description?: string } = {}): Element {
  return element("empty", { title, ...props });
}

export type CodeTone = "added" | "removed" | "context" | "hunk" | "meta";

export interface CodeCell {
  text?: string;
  tone?: CodeTone;
  /** `number`: a line number in a gutter; `omitted`: a gutter with no line. */
  kind?: "text" | "number" | "omitted";
}

export type CodeLine = { text: string; tone?: CodeTone } | { cells: CodeCell[] };

/** Monospace lines; long ones wrap unless `wrap` is false. */
export function code(lines: CodeLine[], props: { wrap?: boolean; minWidth?: number } = {}): Element {
  return element("code", { lines, ...props });
}

/** Its children, scrolling once taller than `maxHeight` pixels. */
export function scroll(children: Child[], props: { maxHeight: number; padding?: Gap }): Element {
  return element("scroll", { ...props, children: kids(children) });
}

export function divider(): Element {
  return { type: "divider" };
}

export function spinner(): Element {
  return { type: "spinner" };
}
