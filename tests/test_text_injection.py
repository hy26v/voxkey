# ABOUTME: Records the missing audio-to-EIS integration-test coverage.
# ABOUTME: The isolated EIS peer cannot validate delivery into a focused desktop client.

import pytest


@pytest.mark.skip(
    reason=(
        "the isolated EIS peer can record protocol events but cannot prove a "
        "real compositor delivered text to the focused application"
    )
)
def test_audio_to_eis_integration_requires_an_eis_capable_portal_mock():
    """Do not substitute retired NotifyKeyboardKeysym logs for EIS coverage."""
    pass
