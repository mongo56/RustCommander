# Markdown sample (H1)

A test file for the viewer's rendered Markdown view (F3 on a `.md` file,
key **5**). Every element the viewer draws appears below at least once.
This paragraph is long enough to wrap in the reading column, so the line
height and the column width (at most about 800 pixels, centred) are easy to
judge when the window is made wider or narrower.

## Inline styles (H2)

Plain text, **bold**, *italic*, ***bold italic***, ~~strikethrough~~,
`inline code`, and **bold with `code` inside**. A line ending in two spaces  
makes a hard break, and so does a backslash\
like this.

Links: [RustCommander on GitHub](https://github.com/mongo56/RustCommander)
(opens in the browser), <https://www.rust-lang.org> (autolink), a relative
link to [the README](../README.md) (opens in this viewer), a link to a
[missing file](nothing-here.md), and an [anchor](#tables) (not followed).

### Heading 3

#### Heading 4

##### Heading 5

###### Heading 6

## Lists

- First bullet
- Second bullet with a longer text that wraps onto a second line when the
  window is narrow, to check the hanging indent
  - Nested bullet
    - Third level
- Back to the first level

1. Numbered one
2. Numbered two
   1. Nested numbered
   2. Another
3. Numbered three

7. A list that starts at seven
8. Eight

- Loose list item one

- Loose list item two, with a second paragraph:

  This paragraph belongs to item two.

## Task list

- [x] Done task
- [ ] Open task
  - [x] Nested done task
  - [ ] Nested open task
- [ ] Task with **bold** and `code`

## Block quotes

> A quote with a left bar in a muted colour.
> It can span several lines and have **styles**.
>
> > A nested quote.
>
> - and a list inside the quote

## Code

Fenced code block:

```rust
fn main() {
    let name = "RustCommander";
    println!("Hello from {name}!");
}
```

Indented code block:

    dir /b *.md
    copy a.md b.md

A very long line in a code block wraps: `aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa`

```
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
```

## Tables

| Key | Action | Notes |
|:----|:------:|------:|
| F3 | View | **bold** header row above |
| 1 | Text mode | left / centre / right aligned columns |
| 5 | Markdown | a longer cell that may wrap onto two lines in a narrow window |

| Single |
|--------|
| column |

## Horizontal rule

Above the rule.

---

Below the rule.

## Images

An image shows only its alt text: ![RustCommander screenshot](screenshots/main.png)
and one without alt text: ![](missing.png).

## Raw HTML (shown as text)

<div align="center">
  <b>Not rendered</b>
</div>

Inline <span style="color:red">HTML</span> stays as text too.
