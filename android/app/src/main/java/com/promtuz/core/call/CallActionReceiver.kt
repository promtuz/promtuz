package com.promtuz.core.call

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import com.promtuz.core.CoreBridge

/** Answer / hang-up buttons on the call notification. */
class CallActionReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        when (intent.action) {
            ACTION_ANSWER -> {
                // Answering needs permission and a foreground start, so route it
                // through the call screen rather than accepting here: a stale or
                // repeated Answer must not throw an uncaught Refused from core.
                CallController.requestAnswer()
                CallActivity.launch(context.applicationContext)
            }
            ACTION_HANGUP -> runCatching { CoreBridge.callHangup() }
        }
    }

    companion object {
        const val ACTION_ANSWER = "com.promtuz.core.call.ANSWER"
        const val ACTION_HANGUP = "com.promtuz.core.call.HANGUP"
    }
}
