package com.promtuz.core.push

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.widget.Toast
import androidx.core.app.RemoteInput
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import timber.log.Timber

class ReplyReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val conversation = intent.getByteArrayExtra("conversation") ?: return
        val text = RemoteInput.getResultsFromIntent(intent)?.getCharSequence(PushNotifier.KEY_REPLY)?.toString()
            ?: return
        PushNotifier.cancelChat(context, conversation.toHex())
        val app = context.applicationContext
        val pending = goAsync()
        CoroutineScope(Dispatchers.IO).launch {
            try {
                // Read before the send, so the reconcile after it does not re-post the chat.
                CoreBridge.markConversationRead(conversation)
                CoreBridge.sendMessage(conversation, text)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                Timber.tag("Push").w(e, "Could not send a reply")
                withContext(Dispatchers.Main) {
                    Toast.makeText(app, "Couldn’t send. Try again.", Toast.LENGTH_LONG).show()
                }
            } finally {
                pending.finish()
            }
        }
    }
}
