package com.promtuz.chat.utils

import android.net.Uri
import android.util.Base64

/**
 * The shareable `https://promtuz.dev/pair#<code>` invite link. `<code>` is the
 * same bytes [com.promtuz.core.CoreBridge.makeInviteQr] returns, base64url-encoded
 * (URL-safe, no padding) in the URL fragment. Same encode used to build a link,
 * same decode used when a `/pair` deeplink opens the app.
 */
object InviteLink {
    /** Intent extra carrying decoded invite bytes between activities (deferred deeplink). */
    const val EXTRA_INVITE = "invite"

    private const val FLAGS = Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP
    private const val PREFIX = "https://promtuz.dev/pair#"

    fun build(inviteBytes: ByteArray): String =
        PREFIX + Base64.encodeToString(inviteBytes, FLAGS)

    /** Invite bytes from a pair deeplink; null if no code or it won't base64url-decode. */
    fun decode(uri: Uri): ByteArray? {
        val code = uri.encodedFragment?.takeIf { it.isNotBlank() }
            ?: uri.getQueryParameter("i")?.takeIf { it.isNotBlank() }
            ?: return null
        return try {
            Base64.decode(code, FLAGS)
        } catch (e: IllegalArgumentException) {
            null
        }
    }
}
