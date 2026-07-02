package com.promtuz.chat

import android.app.Application
import android.content.pm.ApplicationInfo
import com.promtuz.chat.di.appModule
import com.promtuz.chat.di.vmModule
import com.promtuz.chat.utils.logs.AppLog
import com.promtuz.chat.utils.logs.AppLogger
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
        Timber.plant(
            Timber.DebugTree(), AppLogger
        )

        readJNILogs()

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