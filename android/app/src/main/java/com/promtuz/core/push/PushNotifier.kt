package com.promtuz.core.push

import android.Manifest
import android.app.Application
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Intent
import android.content.pm.PackageManager
import androidx.core.app.ActivityCompat
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.Person
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import com.promtuz.chat.LauncherActivity
import com.promtuz.chat.R
import com.promtuz.core.CoreBridge
import com.promtuz.core.adapter.CoreEventBus
import com.promtuz.core.adapter.IncomingMessage
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.toHex
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch

/**
 * Turns delivered incoming messages into notifications — one
 * [NotificationCompat.MessagingStyle] per chat, grouped under a summary. A
 * projection of what libcore delivered, so nothing here decrypts or trusts the
 * wake payload. Suppressed while the app is foregrounded (you're already looking).
 */
object PushNotifier {
    private const val SUMMARY_ID = 1

    private lateinit var app: Application
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private var names: Map<String, String> = emptyMap()

    @Volatile
    private var foreground = false

    fun start(application: Application) {
        app = application
        Notifications.ensureChannels(application)
        ProcessLifecycleOwner.get().lifecycle.addObserver(object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) { foreground = true; clear() }
            override fun onStop(owner: LifecycleOwner) { foreground = false }
        })
        scope.launch {
            CoreEventBus.incoming.collect { msg -> if (!foreground) post(msg) }
        }
    }

    private suspend fun post(msg: IncomingMessage) {
        if (ActivityCompat.checkSelfPermission(app, Manifest.permission.POST_NOTIFICATIONS)
            != PackageManager.PERMISSION_GRANTED
        ) return

        if (msg.peerHex !in names) {
            names = runCatching { CoreBridge.contacts().associate { it.ipk.toHex() to it.name } }
                .getOrDefault(names)
        }
        val them = Person.Builder().setName(names[msg.peerHex] ?: "New message").setKey(msg.peerHex).build()

        // Hydrate from the DB (not in-memory state) so the notification survives
        // process death — an FCM cold-wake shows the recent thread, not just the
        // one message that woke us.
        val recent = runCatching { CoreBridge.messages(msg.peerHex.fromHex(), MAX_LINES) }
            .getOrDefault(emptyList())
            .filterNot { it.deleted }
            .sortedBy { it.timestamp }

        val style = NotificationCompat.MessagingStyle(Person.Builder().setName("You").build())
        if (recent.isEmpty()) {
            style.addMessage(msg.content, msg.timestampMs, them) // fallback if the DB write hasn't landed
        } else {
            recent.forEach {
                style.addMessage(it.content, it.timestamp.toLong() * 1000, if (it.outgoing) null else them)
            }
        }

        val nm = NotificationManagerCompat.from(app)
        nm.notify(
            notifId(msg.peerHex),
            NotificationCompat.Builder(app, Notifications.MESSAGES_CHANNEL)
                .setSmallIcon(R.drawable.i_notifications)
                .setStyle(style)
                .setGroup(Notifications.GROUP_KEY)
                .setAutoCancel(true)
                .setContentIntent(openApp())
                .build(),
        )
        nm.notify(
            SUMMARY_ID,
            NotificationCompat.Builder(app, Notifications.MESSAGES_CHANNEL)
                .setSmallIcon(R.drawable.i_notifications)
                .setGroup(Notifications.GROUP_KEY)
                .setGroupSummary(true)
                .setAutoCancel(true)
                .build(),
        )
    }

    private fun openApp(): PendingIntent {
        val intent = Intent(app, LauncherActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP)
        return PendingIntent.getActivity(
            app, 0, intent, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
    }

    private fun clear() {
        if (!::app.isInitialized) return
        // Cancel the message notifications (incl. the summary — all carry GROUP_KEY) but NOT the drain
        // worker's foreground-service notice on API < 31: it has no group, and cancelling a live FGS
        // notification is illegal.
        val nm = app.getSystemService(NotificationManager::class.java)
        nm.activeNotifications
            .filter { it.notification.group == Notifications.GROUP_KEY }
            .forEach { nm.cancel(it.id) }
    }

    /** Deterministic per-peer id (so a cold FCM wake updates the same chat's notification instead of
     *  duplicating), kept clear of the reserved summary (1) / drain-FGS (42) ids. */
    private fun notifId(peerHex: String): Int {
        val h = peerHex.hashCode() and 0x7FFF_FFFF
        return if (h < RESERVED_MAX) h + RESERVED_MAX else h
    }

    private const val MAX_LINES = 8
    private const val RESERVED_MAX = 100
}
