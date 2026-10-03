package com.promtuz.chat.utils

import android.net.Uri
import com.promtuz.core.CoreBridge

object InviteLink {
    const val EXTRA_INVITE = "invite"

    fun build(inviteBytes: ByteArray): String = CoreBridge.inviteLink(inviteBytes)

    fun decode(uri: Uri): ByteArray? = CoreBridge.inviteFromLink(uri.toString())
}
