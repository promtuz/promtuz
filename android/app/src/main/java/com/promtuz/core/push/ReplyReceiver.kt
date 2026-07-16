package com.promtuz.core.push

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import androidx.core.app.RemoteInput
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch

/**
 * Inline "Reply" action on a message notification. Cancels the notification UP FRONT so the
 * RemoteInput spinner resolves immediately — the send may force a slow reconnect but is durable
 * via the outbox — then marks read + sends in the background.
 */
class ReplyReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val peer = intent.getByteArrayExtra("peer") ?: return
        val text = RemoteInput.getResultsFromIntent(intent)?.getCharSequence(PushNotifier.KEY_REPLY)?.toString()
            ?: return
        // Resolve the spinner NOW, before the (possibly slow) send. Mark read before send so a
        // post-send reconcile doesn't briefly re-post the now-read chat.
        PushNotifier.cancelChat(context, peer.toHex())
        val pending = goAsync()
        CoroutineScope(Dispatchers.IO).launch {
            try {
                CoreBridge.markConversationRead(peer)
                CoreBridge.sendMessage(peer, text)
            } finally {
                pending.finish()
            }
        }
    }
}
