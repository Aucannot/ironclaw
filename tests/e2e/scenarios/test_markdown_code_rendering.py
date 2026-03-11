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
