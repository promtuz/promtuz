package com.promtuz.chat.utils

import android.net.Uri
import com.promtuz.core.CoreBridge

/**
 * The shareable invite link. The URL contract lives in core, so both ends of
 * it move together and iOS reuses it — a client that built links one way and
 * parsed them another would only find out when someone failed to pair.
 */
object InviteLink {
    /** Intent extra carrying decoded invite bytes between activities (deferred deeplink). */
    const val EXTRA_INVITE = "invite"

    fun build(inviteBytes: ByteArray): String = CoreBridge.inviteLink(inviteBytes)

    /** Invite bytes from a pair deeplink; null if no code or it won't decode. */
    fun decode(uri: Uri): ByteArray? = CoreBridge.inviteFromLink(uri.toString())
}
