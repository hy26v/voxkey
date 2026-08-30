# ABOUTME: Records the missing coverage for transcripts the keyboard layout cannot type.
# ABOUTME: Survival of a refused injection is proven in src/eis.rs against a real EIS server.

import pytest


@pytest.mark.skip(
    reason=(
        "a real focused desktop surface is still required for the manual gate. "
        "Protocol-level refusal and session survival are covered by "
        "src/eis.rs::text_the_layout_cannot_type_leaves_the_session_usable, "
        "which drives an isolated EIS peer"
    )
)
def test_untypeable_transcript_survival_requires_an_eis_capable_portal():
    """Keep this coverage gap explicit.

    Speech models routinely emit characters a plain layout has no key for -- the
    curly apostrophe in "don't" is the common one. Voxkey must report that it
    could not type them and stay ready for the next dictation, rather than
    treating it as a broken input session and shutting down.
    """
    pass
