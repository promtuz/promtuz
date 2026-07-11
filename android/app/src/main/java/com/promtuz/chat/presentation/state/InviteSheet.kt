package com.promtuz.chat.presentation.state

/** Confirmation-sheet state for an incoming invite link. `null` = no sheet shown. */
sealed interface InviteSheet {
    /** previewInvite() in flight. */
    data object Decoding : InviteSheet

    /** Decoded — show the prompt tailored by the flags. `expiryMs` drives a live
     *  countdown; `isSelf` means it's our own link (pairing refused). */
    data class Confirm(
        val bytes: ByteArray,
        val ipk: ByteArray,
        val name: String,
        val alreadyContact: Boolean,
        val expiryMs: Long,
        val isSelf: Boolean,
    ) : InviteSheet

    /** Pairing in flight — the welcome is being published to their homes. */
    data class Pairing(val name: String) : InviteSheet

    /** The contact appeared (PENDING): the pair is sent; it confirms when they're online. */
    data class Added(val ipk: ByteArray, val name: String) : InviteSheet

    /** Couldn't reach them (KeyPackage not published / timed out). Retryable. */
    data class Unreachable(val bytes: ByteArray, val name: String) : InviteSheet

    /** Malformed link or previewInvite() threw. */
    data object Invalid : InviteSheet
}
