package com.promtuz.core.push

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch

/**
 * "Mark read" action on a message notification: clears unread. The resulting DB write
 * rings [CoreEventBus.dbChanged], and PushNotifier's reconcile dismisses the notif.
 */
// ponytail: read-on-another-device dismiss lives in libcore (a read receipt → local mark) — out of scope here.
class MarkReadReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val peer = intent.getByteArrayExtra("peer") ?: return
        PushNotifier.cancelChat(context, peer.toHex()) // dismiss immediately, before the async mark
        val pending = goAsync()
        CoroutineScope(Dispatchers.IO).launch {
            try {
                CoreBridge.markConversationRead(peer)
            } finally {
                pending.finish()
            }
        }
    }
}
