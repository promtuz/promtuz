package com.promtuz.core

import android.app.Service
import android.content.Intent
import android.os.IBinder

class AppCloseService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onTaskRemoved(rootIntent: Intent?) {
        CoreBridge.onTaskRemoved()
        stopSelf()
        super.onTaskRemoved(rootIntent)
    }
}
