package com.promtuz.core.push

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch

class MarkReadReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val conversation = intent.getByteArrayExtra("conversation") ?: return
        PushNotifier.cancelChat(context, conversation.toHex())
        val pending = goAsync()
        CoroutineScope(Dispatchers.IO).launch {
            try {
                CoreBridge.markConversationRead(conversation)
            } finally {
                pending.finish()
            }
        }
    }
}
