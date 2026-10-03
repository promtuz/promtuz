package com.promtuz.chat

import android.app.Application
import android.content.Intent
import android.content.pm.ApplicationInfo
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import com.promtuz.chat.backup.BackupWorker
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.di.appModule
import com.promtuz.chat.di.vmModule
import com.promtuz.chat.ui.appearance.AppearanceStore
import com.promtuz.chat.update.UpdateRepository
import com.promtuz.chat.update.UpdateWorker
import com.promtuz.chat.utils.logs.AppLog
import com.promtuz.chat.utils.logs.AppLogger
import com.promtuz.core.CoreBridge
import com.promtuz.core.CoreInitializer
import com.promtuz.core.AppCloseService
import com.promtuz.core.push.PushRegistrationWorker
import com.promtuz.core.push.PushNotifier
import com.promtuz.core.PresenceStore
import com.promtuz.core.adapter.CoreEventBus
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.launch
import org.koin.android.ext.koin.androidContext
import org.koin.android.ext.koin.androidLogger
import org.koin.core.context.startKoin
import timber.log.Timber

class Promtuz : Application() {
    private fun readJNILogs() {
        CoroutineScope(Dispatchers.IO).launch {
            val pid = android.os.Process.myPid()
            // Only this process's core tag, so other apps and the device log never reach the diagnostic buffer.
            val pattern = Regex("^([VDIWEF])/core\\s*\\(\\s*\\d+\\):\\s?(.*)$")
            runCatching {
                val proc = Runtime.getRuntime().exec(
                    arrayOf("logcat", "--pid=$pid", "-v", "brief", if (isDebuggable()) "core:D" else "core:I", "*:S")
                )
                try {
                    proc.inputStream.bufferedReader().useLines { lines -> lines.forEach { line ->
                        val match = pattern.matchEntire(line) ?: return@forEach
                        AppLogger.push(AppLog(System.currentTimeMillis(), AppLog.charPriority(match.groupValues[1][0]),
                            "core", match.groupValues[2], null))
                    } }
                } finally {
                    proc.destroy()
                }
            }.onFailure {
                AppLogger.push(AppLog(System.currentTimeMillis(), 5, "Logs", "Native logs unavailable", it))
            }
        }
    }

    override fun onCreate() {
        AppLogger.minimumPriority = if (isDebuggable()) 2 else 4
        Timber.plant(AppLogger)
        if (isDebuggable()) Timber.plant(Timber.DebugTree())
        readJNILogs()

        // Seeds last-known presence before core starts firing deltas.
        PresenceStore.init(this)
        CoreEventBus.hydratePresence(PresenceStore.seed())

        PushNotifier.start(this)
        com.promtuz.core.call.CallController.init(this)
        com.promtuz.core.call.CallVideoManager.init(this)
        CoreInitializer.start(this)
        BackupWorker.start(this)
        AppearanceStore.init(this)

        PushRegistrationWorker.enqueue(this)

        CoroutineScope(Dispatchers.IO).launch {
            CoreEventBus.presenceByPeer.collectLatest { map ->
                delay(1500)
                PresenceStore.save(map, System.currentTimeMillis())
            }
        }

        val updates: UpdateRepository = startKoin {
            androidLogger()
            androidContext(this@Promtuz)
            modules(appModule, vmModule)
        }.koin.get()
        UpdateWorker.schedule(this)

        ProcessLifecycleOwner.get().lifecycle.addObserver(object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) {
                startService(Intent(this@Promtuz, AppCloseService::class.java))
                CoreBridge.onForeground()
                CoreBridge.setPresence(idle = false)
                updates.check()
            }

            override fun onStop(owner: LifecycleOwner) = CoreBridge.setPresence(idle = true)
        })

        super.onCreate()
    }

    private fun isDebuggable(): Boolean {
        return 0 != applicationInfo.flags and ApplicationInfo.FLAG_DEBUGGABLE
    }
}
