package com.promtuz.chat.presentation.state

sealed interface InviteSheet {
    data object Decoding : InviteSheet

    /** Self-pairing is not detected here; core refuses it on Add and the reason surfaces as [Invalid]. */
    data class Confirm(
        val bytes: ByteArray,
        val ipk: ByteArray,
        val name: String,
        val alreadyContact: Boolean,
        val expiryMs: Long,
    ) : InviteSheet

    data class Pairing(val name: String) : InviteSheet

    /** Pending: it confirms when they are next online. */
    data class Added(val ipk: ByteArray, val name: String) : InviteSheet

    data class Unreachable(val bytes: ByteArray, val name: String) : InviteSheet

    data class Invalid(val message: String = "This invite link is invalid.") : InviteSheet
}
