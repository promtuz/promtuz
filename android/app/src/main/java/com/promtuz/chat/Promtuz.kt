package com.promtuz.chat

import android.app.Application
import android.content.pm.ApplicationInfo
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import com.promtuz.chat.backup.BackupWorker
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.di.appModule
import com.promtuz.chat.di.vmModule
import com.promtuz.chat.ui.appearance.AppearanceStore
import com.promtuz.chat.utils.logs.AppLog
import com.promtuz.chat.utils.logs.AppLogger
import com.google.firebase.messaging.FirebaseMessaging
import com.promtuz.core.CoreBridge
import com.promtuz.core.CoreInitializer
import com.promtuz.core.push.PushNotifier
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import org.koin.android.ext.koin.androidContext
import org.koin.android.ext.koin.androidLogger
import org.koin.core.context.startKoin
import timber.log.Timber
import java.util.Calendar

class Promtuz : Application() {
    private fun readJNILogs() {
        CoroutineScope(Dispatchers.IO).launch {
            val pid = android.os.Process.myPid()
            val proc = Runtime.getRuntime().exec(
                arrayOf("logcat", "--pid=$pid")
            )

            proc.inputStream.bufferedReader().forEachLine { line ->
                val head = line.substringBefore(" : ")
                var msg = line.substringAfter(" : ")
                if (!msg.startsWith("core:")) return@forEachLine

                val parts = head.split(Regex("\\s+"))

                val priority = AppLog.charPriority(parts[4][0])
                val tag = msg.substringBefore(": ")
                msg = msg.substringAfter(": ")

                val time = Calendar.getInstance().timeInMillis

                AppLogger.push(AppLog(time, priority, tag, msg, null))
            }
        }
    }

    override fun onCreate() {
        // Security: no DebugTree / logcat scraper in release builds.
        if (isDebuggable()) {
            Timber.plant(Timber.DebugTree(), AppLogger)
            readJNILogs()
        }

        CoreInitializer.start()
        BackupWorker.start(this)
        AppearanceStore.init(this)
        ChatPrefs.init(this)

        // Push: post notifications from delivered messages, and hand libcore the current FCM token
        // (onNewToken won't re-fire if unchanged). registerPushToken stores it and re-registers on
        // each relay connect, so calling before the first connect is fine.
        PushNotifier.start(this)
        FirebaseMessaging.getInstance().token.addOnSuccessListener { token ->
            CoreBridge.registerPushToken(token.toByteArray())
        }

        // Foreground → nudge core for an instant reconnect (the raised idle
        // timeout means most app switches never dropped the connection) and go
        // Active. Background → assert Idle (the last packet before we freeze).
        ProcessLifecycleOwner.get().lifecycle.addObserver(object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) {
                CoreBridge.onForeground()
                CoreBridge.setPresence(idle = false)
            }

            override fun onStop(owner: LifecycleOwner) = CoreBridge.setPresence(idle = true)
        })

        startKoin {
            androidLogger()
            androidContext(this@Promtuz)
            modules(
                appModule, vmModule
            )
        }

        super.onCreate()
    }

    private fun isDebuggable(): Boolean {
        return 0 != applicationInfo.flags and ApplicationInfo.FLAG_DEBUGGABLE
    }
}