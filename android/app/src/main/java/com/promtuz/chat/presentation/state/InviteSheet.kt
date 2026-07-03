package com.promtuz.chat.presentation.state

/** Confirmation-sheet state for an incoming invite link. `null` = no sheet shown. */
sealed interface InviteSheet {
    /** previewInvite() in flight. */
    data object Decoding : InviteSheet

    /** Decoded — show the prompt tailored by [alreadyContact] / [expired]. */
    data class Confirm(
        val bytes: ByteArray,
        val ipk: ByteArray,
        val name: String,
        val alreadyContact: Boolean,
        val expired: Boolean,
    ) : InviteSheet

    /** Malformed link or previewInvite() threw. */
    data object Invalid : InviteSheet

    /** pairFromQr() queued — brief success before auto-dismiss. */
    data class Added(val name: String) : InviteSheet
}
