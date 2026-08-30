# ABOUTME: Records the missing Unicode audio-to-EIS integration-test coverage.
# ABOUTME: EurKEY mapping itself is exercised against a protocol-level EIS peer in Rust.

import pytest


@pytest.mark.skip(
    reason=(
        "the isolated EIS peer cannot prove Unicode delivery through a real "
        "compositor into a focused application"
    )
)
def test_unicode_audio_to_eis_requires_an_eis_capable_portal_mock():
    """Keep this coverage gap explicit instead of testing the retired API."""
    pass
