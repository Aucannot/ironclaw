"""Scenario: Markdown code rendering enhancements should be stable and usable."""

from helpers import SEL


MARKDOWN_PAYLOAD = """# Rendering check

Inline formula: $E=mc^2$

```rust
fn main() {
    println!("hello");
}
```

```python
def add(a, b):
    return a + b
```
"""


async def test_code_blocks_have_headers_and_no_duplicates(page):
    """Code blocks should get language chips + copy buttons exactly once."""
    await page.evaluate("content => addMessage('assistant', content)", MARKDOWN_PAYLOAD)

    assistant_msg = page.locator(SEL["message_assistant"]).last
    await assistant_msg.wait_for(state="visible", timeout=5000)

    # Initial render should decorate both fenced blocks.
    headers = assistant_msg.locator(".code-block-header")
    await headers.first.wait_for(state="visible", timeout=5000)
    assert await headers.count() == 2

    langs = assistant_msg.locator(".code-lang")
    assert await langs.nth(0).text_content() == "rust"
    assert await langs.nth(1).text_content() == "python"

    copy_buttons = assistant_msg.locator("button.copy-btn")
    assert await copy_buttons.count() == 2

    # Simulate streaming append; enhancement should remain idempotent.
    await page.evaluate("chunk => appendToLastAssistant(chunk)", "\nstream tail")
    await page.wait_for_timeout(200)

    assert await assistant_msg.locator(".code-block-header").count() == 2
    assert await assistant_msg.locator("button.copy-btn").count() == 2


async def test_code_block_header_is_not_nested_in_pre(page):
    """Code block header should be sibling of <pre>, not nested inside it."""
    await page.evaluate("content => addMessage('assistant', content)", MARKDOWN_PAYLOAD)

    assistant_msg = page.locator(SEL["message_assistant"]).last
    await assistant_msg.wait_for(state="visible", timeout=5000)

    nested_headers = assistant_msg.locator("pre .code-block-header")
    assert await nested_headers.count() == 0

    wrapped_pres = assistant_msg.locator(".code-block-wrapper > pre")
    assert await wrapped_pres.count() == 2


async def test_markdown_links_are_hardened(page):
    """Dangerous URL schemes should be stripped and _blank links hardened."""
    payload = """[unsafe](javascript:alert(1))

<a href="https://example.com" target="_blank">safe</a>"""
    await page.evaluate("content => addMessage('assistant', content)", payload)

    assistant_msg = page.locator(SEL["message_assistant"]).last
    await assistant_msg.wait_for(state="visible", timeout=5000)

    assert await assistant_msg.locator('a[href^="javascript:"]').count() == 0

    external_link = assistant_msg.locator('a[target="_blank"]').first
    rel = (await external_link.get_attribute('rel')) or ''
    assert 'noopener' in rel
    assert 'noreferrer' in rel
