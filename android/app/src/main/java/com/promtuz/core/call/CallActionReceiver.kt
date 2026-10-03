package com.promtuz.core.call

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import com.promtuz.core.CoreBridge

/** The notification's Hang up and Decline buttons. Answer opens the call screen instead. */
class CallActionReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != ACTION_HANGUP) return
        intent.getByteArrayExtra(CallActivity.EXTRA_CALL)?.let(CoreBridge::callHangup)
    }

    companion object {
        const val ACTION_HANGUP = "com.promtuz.core.call.HANGUP"
    }
}
