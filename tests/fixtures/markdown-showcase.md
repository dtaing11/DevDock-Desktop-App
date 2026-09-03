# Markdown showcase

This file exercises **every construct** the app's Markdown renderer supports,
plus the ones it *deliberately* does not. Open it in DevDock, select it in
the Changes list, and turn on **Rendered** to see the whole surface at once.

It is also a test fixture: `markdown.rs` parses and renders this exact file,
so anything that breaks here breaks a test.

---

## Headings

Levels 1 to 3 step down in size, and 4 to 6 share body size in the heavier
face — a review or a README rarely needs six visual tiers, and weight tells a
small heading from a paragraph without another size for each. Only level 1 gets a rule under it, since
giving every level 2 one turns a normal document into a stack of lines.

# Level 1
## Level 2
### Level 3
#### Level 4
##### Level 5
###### Level 6

####### Seven hashes is not a heading, so this stays a paragraph.

#No space after the hash is also not a heading.

---

## Paragraphs

A paragraph is a run of lines with no blank line between them. This sentence
and the next two are written across several source lines, and the renderer
joins them into one flowing paragraph rather than breaking where the source
happens to wrap.

A blank line starts a new paragraph.

Two spaces at the end of a line do **not** force a hard break — trailing
whitespace is trimmed, so this line and the next are one paragraph.

---

## Inline formatting

Ordinary text, then **bold with asterisks**, then __bold with underscores__,
then *italic with asterisks*, then _italic with underscores_, then `inline
code`, then a [link to the repository](https://github.com/dtaing11/DevDock-Desktop-App).

**Bold** is a real semibold face and *italic* a real slanted one — the app
bundles all three weights of Inter, because a renderer with only a regular
face can express emphasis with nothing but colour. `Code` is monospace,
tinted, and sits on a chip.

Emphasis inside a sentence works mid-word too: un**believ**able.

Inline code takes precedence over emphasis, so `**this stays literal**` keeps
its asterisks, and `_so does this_`.

Unmatched markers are printed literally instead of eating the rest of the
line: a lone * asterisk, a lone _ underscore, an unclosed **bold, and a
stray ` backtick all survive as text.

A link needs both halves: [this is a link](https://example.com), while
[this is not] and [this either](unclosed stay as plain text. A bare URL like
https://example.com is not turned into a link.

Markup inside a link label is **not** parsed, so
[**these asterisks** stay visible](https://example.com) in the rendered link.
Outside a link, the same text — [**this is emphasised**] — is parsed normally,
because without a `(url)` it is not a link at all.

---

## Lists

Unordered lists accept all three bullet markers:

- A dash bullet
* An asterisk bullet
+ A plus bullet

Ordered lists accept both separators, and the number you wrote is the number
shown:

1. First
2. Second
7) Seventh, because the source says seven
12. Two-digit numbers work

Nesting is by indentation, two spaces per level:

- Top level
  - One level in
    - Two levels in
  - Back out one
- Top level again

A wrapped source line belongs to the item above it, the way every README
writes a long bullet:

- A bullet whose text is long enough that it has to be wrapped in the source,
  continued on the next line with two spaces of indentation, which stays part
  of this item rather than breaking out into its own paragraph

List items carry inline formatting:

- **Bold** in a list item
- `code` in a list item
- A [link](https://example.com) in a list item
- A longer item that runs past the width of the panel so you can check that
  wrapped list text lines up the way you would expect it to

---

## Blockquotes

> A blockquote is dimmed text with a rule down its left side.

> Consecutive quoted lines are one blockquote, so this line and the next
> share a single rule down the side.

>A quote without a space after the marker still works.

> Quotes carry **inline formatting**, `code`, and [links](https://example.com).

---

## Thematic breaks

Three or more dashes, asterisks, or underscores, alone on a line:

---

***

___

- - -

Anything else on the line makes it not a rule, so `--- like this ---` is a
paragraph.

---

## Code blocks

Fenced code blocks are highlighted by their info string. Rust:

```rust
/// Resolves one conflicted file and stages the result.
pub fn resolve(&self, file: &str, resolution: &Resolution) -> Result<()> {
    match resolution {
        Resolution::Ours => self.git(&["checkout", "--ours", "--", file]).map(drop)?,
        Resolution::Manual(content) => std::fs::write(self.root.join(file), content)?,
        _ => return Err(GitError("unhandled".into())),
    }
    self.git(&["add", "--", file]).map(drop)
}
```

Python:

```python
def resolve(path: str, *, staged: bool = False) -> None:
    """Stage a resolution."""
    if not path:
        raise ValueError("path is required")  # comment
    subprocess.run(["git", "add", "--", path], check=True)
```

Go:

```go
func Halve(n int) (int, error) {
    if n < 0 {
        return 0, fmt.Errorf("negative: %d", n)
    }
    return n / 2, nil
}
```

C:

```c
#include <stdio.h>

int total(int *values, int count) {
    int sum = 0;
    for (int i = 0; i < count; i++) { sum += values[i]; }
    return sum;
}
```

JavaScript and TypeScript:

```js
const files = status.files.filter((f) => !f.staged);
console.log(`${files.length} unstaged`);
```

```ts
export function elide(path: string, max: number): string {
  return path.length <= max ? path : `…/${path.slice(-max)}`;
}
```

Java:

```java
public final class Resolver {
    public static void main(String[] args) {
        System.out.println("resolved " + args.length + " files");
    }
}
```

Dart:

```dart
Future<void> resolve(String path) async {
  final result = await Process.run('git', ['add', '--', path]);
  if (result.exitCode != 0) throw StateError('failed');
}
```

TOML, which is what this project's own config uses:

```toml
[[job]]
name = "tests"
commands = ["cargo test"]

[review]
run = true
fail_on = "high"        # low | medium | high
```

JSON:

```json
{
  "summary": "one problem found",
  "findings": [{"file": "src/git.rs", "line": 42, "severity": "high"}],
  "clean": false
}
```

YAML:

```yaml
name: CI
on: [push]
jobs:
  test:
    runs-on: ubuntu-latest
```

Shell:

```sh
# Install and run
cargo build --release
./target/release/devdock status | grep -c '^'
```

A fence with **no info string** falls back to plain text:

```
no language here
    indentation is preserved exactly
        even at three levels
```

A fence with an **unknown language** also falls back rather than failing:

```brainfuck
++++++++[>++++[>++>+++>+++>+<<<<-]>+>+>->>+[<]<-]
```

Blank lines *inside* a code block keep their height:

```rust
fn first() {}

fn second() {}


fn third_after_two_blanks() {}
```

Markdown inside a code block is shown as source, not rendered:

```markdown
# This heading stays literal
- so does this bullet
**and this bold**
```

---

## What is deliberately not supported

The renderer covers what a README and an AI code review actually use. Every
construct below degrades to readable text rather than breaking the document —
that is the behaviour to check, not the absence of the feature.

Tables are not parsed. Because the rows have no blank lines between them,
they join into a single run-together paragraph — readable, but not a table:

| Column | Meaning |
|--------|---------|
| `run`  | Review before every push |
| `fail_on` | Lowest severity that blocks |

Task lists render as ordinary bullets with the brackets intact:

- [ ] Unchecked item
- [x] Checked item

Images render as the link text with a stray leading exclamation mark:

![A screenshot](assets/screenshot.png)

Strikethrough is not parsed, so ~~this~~ keeps its tildes.

Setext headings are not recognised, so the underline joins the paragraph:

Setext heading
==============

Inline HTML is printed literally: <strong>this stays as source</strong>.

Footnotes[^1] keep their markers.

[^1]: And the definition renders as a normal paragraph.

A hard tab inside a line	is kept as written rather than expanded.

---

## Long document behaviour

Everything above lives in one scroll area with a capped measure, so prose does
not stretch to the full width of a wide window. Files over 1 MB are refused
with a message rather than rendered, and a file that is not in the working
tree — a deletion, say — says so instead of showing an empty pane.

The last section is deliberately an **unterminated code fence**. The parser
closes it at the end of the file rather than swallowing the document, so
everything above still renders. Nothing may be added below it.

```rust
fn unterminated() {
    // This fence is never closed. It must stay the last block in the file.
}
