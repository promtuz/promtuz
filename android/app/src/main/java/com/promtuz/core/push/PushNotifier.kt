package com.promtuz.core.push

import android.Manifest
import android.app.Activity
import android.app.Application
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Typeface
import android.os.Bundle
import android.os.SystemClock
import androidx.core.app.ActivityCompat
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.Person
import androidx.core.app.RemoteInput
import androidx.core.content.LocusIdCompat
import androidx.core.content.pm.ShortcutInfoCompat
import androidx.core.content.pm.ShortcutManagerCompat
import androidx.core.graphics.drawable.IconCompat
import com.promtuz.chat.LauncherActivity
import com.promtuz.chat.R
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.data.NotifBuzz
import com.promtuz.chat.domain.model.mediaLabel
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.core.CoreBridge
import com.promtuz.core.adapter.CoreEventBus
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.debounce
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import timber.log.Timber

/** Reconciles the notification shade with unread messages and persisted alert state. */
object PushNotifier {
    private const val SUMMARY_ID = 1

    /** RemoteInput result key; shared with [ReplyReceiver]. */
    const val KEY_REPLY = "reply_text"

    /** Notification-tap extras: which chat to open (hex IPK + display name). */
    const val EXTRA_CONVERSATION = "chat_conversation_hex"
    const val EXTRA_CONV_NAME = "chat_conversation_name"

    private lateinit var app: Application
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    /** Conversation hex → the title to show for it. */
    private var names: Map<String, String> = emptyMap()

    /** Contact hex → their name, for attributing individual lines in a group. */
    private var senderNames: Map<String, String> = emptyMap()

    /** Conversation hexes that are groups — they attribute lines per sender. */
    private var groupConvs: Set<String> = emptySet()
    private var mutedConvs: Set<String> = emptySet()
    private val reconcileLock = Mutex()
    private val rendered = mutableMapOf<String, ChatSnapshot>()

    private data class ChatSnapshot(
        val name: String,
        val count: Int,
        val group: Boolean,
        val preview: Boolean,
        val buzz: NotifBuzz,
        val lines: List<LineSnapshot>,
    )

    private data class LineSnapshot(
        val id: String,
        val text: String,
        val mediaKind: Int,
        val timestamp: ULong,
        val author: String?,
    )

    /** Gates posting — reconcile still runs foregrounded, but only to clear read chats' notifs. */
    @Volatile
    private var foreground = false

    fun start(application: Application) {
        app = application
        Notifications.ensureChannels(application)
        application.registerActivityLifecycleCallbacks(object : Application.ActivityLifecycleCallbacks {
            private val resumed = mutableSetOf<Activity>()
            override fun onActivityResumed(activity: Activity) {
                resumed.add(activity)
                foreground = true
                scope.launch { reconcileSafely() }
            }
            override fun onActivityPaused(activity: Activity) {
                resumed.remove(activity)
                foreground = resumed.isNotEmpty()
                if (!foreground) scope.launch { reconcileSafely() }
            }
            override fun onActivityCreated(activity: Activity, state: Bundle?) = Unit
            override fun onActivityStarted(activity: Activity) = Unit
            override fun onActivityStopped(activity: Activity) = Unit
            override fun onActivitySaveInstanceState(activity: Activity, state: Bundle) = Unit
            override fun onActivityDestroyed(activity: Activity) = Unit
        })
        // Subscribe before core starts. Alerts come from durable message state,
        // so a cold start or a dropped transient event cannot lose an arrival.
        scope.launch(start = CoroutineStart.UNDISPATCHED) {
            CoreEventBus.dbChanged.filter { "messages" in it || "contacts" in it || "prefs" in it }
                .debounce(150L).collect { reconcileSafely() }
        }
        scope.launch { reconcileSafely() }
    }

    suspend fun refresh() = reconcileLock.withLock { reconcile() }

    private suspend fun reconcileSafely() {
        try {
            refresh()
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            Timber.tag("Push").w(e, "Could not update message notifications")
        }
    }

    private suspend fun reconcile() {
        if (!::app.isInitialized) return
        if (ActivityCompat.checkSelfPermission(app, Manifest.permission.POST_NOTIFICATIONS)
            != PackageManager.PERMISSION_GRANTED
        ) return

        val nm = app.getSystemService(NotificationManager::class.java)

        // Master off: nuke our whole group (children + summary) and post nothing — flipping the switch
        // off should silence AND clear the shade, not just stop future buzzes.
        val enabled = ChatPrefs.notifEnabled
        if (!enabled) {
            nm.activeNotifications
                .filter { it.notification.group == Notifications.GROUP_KEY }
                .forEach { nm.cancel(it.id) }
            rendered.clear()
        }

        val counts = CoreBridge.unreadCounts().associate { it.conversationId.toHex() to it.count.toInt() }
        val contacts = CoreBridge.contacts().associate { it.ipk.toHex() to it.name }
        val convs = CoreBridge.listConversations()
        senderNames = contacts
        names = convs.associate { c ->
            c.id.toHex() to (if (c.kind.toInt() == 1) c.displayName else contacts[c.peer?.toHex()].orEmpty())
                .ifBlank { "New message" }
        }
        groupConvs = convs.filter { it.kind.toInt() == 1 }.map { it.id.toHex() }.toSet()
        mutedConvs = convs.filter { it.muted }.map { it.id.toHex() }.toSet()
        rendered.keys.retainAll(counts.keys)
        for (conv in counts.keys) {
            if (!enabled || foreground || conv in mutedConvs) {
                val pending = CoreBridge.pendingNotificationIds(conv.fromHex())
                if (pending.isNotEmpty() && (!enabled || foreground || conv in mutedConvs)) {
                    CoreBridge.markNotified(pending)
                }
            }
        }

        if (!enabled) return

        // Muted chats drop out entirely: they neither post nor stay in `live`, so muting a chat also
        // clears any notif it already had.
        val visible = counts.filterKeys { it !in mutedConvs }

        // Dismiss per-chat notifs whose chat is no longer unread (read from any surface) or now muted.
        // Unconditional (runs foregrounded too), so an in-app read or a mute clears the shade. GROUP_KEY
        // filter spares the drain worker's foreground-service notice (SYNC id 42, no group).
        val live = visible.keys.map(::notifId).toSet()
        nm.activeNotifications
            .filter { it.notification.group == Notifications.GROUP_KEY && it.id != SUMMARY_ID && it.id !in live }
            .forEach { nm.cancel(it.id) }
        rendered.keys.retainAll(visible.keys)

        if (visible.isEmpty()) {
            nm.cancel(SUMMARY_ID)
            return
        }

        if (!foreground) {
            for ((convHex, n) in visible) postChat(convHex, n)
        }
    }

    private suspend fun postChat(convHex: String, n: Int) {
        val conv = convHex.fromHex()
        val displayName = names[convHex] ?: "New message"

        // The newest n incoming ≈ the unread ones (read is a high-water-mark). Core
        // picks and orders them — it is a question about messages, and iOS would
        // otherwise write the same filter-and-sort over a full page of rows.
        val pending = CoreBridge.pendingNotificationIds(conv)
        val recent = CoreBridge.recentIncoming(conv, MAX_LINES).takeLast(n)
        if (foreground) return
        // All newest-window rows deleted-for-everyone while count>0: nothing to paint, so clear this
        // chat's own notif (a bare return would strand a stale one reconcile can't repaint or cancel).
        if (recent.isEmpty()) { nm().cancel(notifId(convHex)); return }

        val readPI = PendingIntent.getBroadcast(
            app, -notifId(convHex),
            Intent(app, MarkReadReceiver::class.java).putExtra("conversation", conv),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val readAction = NotificationCompat.Action.Builder(R.drawable.i_check, "Mark read", readPI)
            .setSemanticAction(NotificationCompat.Action.SEMANTIC_ACTION_MARK_AS_READ)
            .setShowsUserInterface(false)
            .build()

        val isGroup = groupConvs.contains(convHex)
        val newest = recent.last().timestamp.toLong()
        val mode = ChatPrefs.notifBuzz
        val alertThisChat = pending.isNotEmpty()
        val snapshot = ChatSnapshot(displayName, n, isGroup, ChatPrefs.notifPreview, mode,
            recent.map { message ->
                LineSnapshot(message.id, message.content, message.mediaKind.toInt(), message.timestamp,
                    message.senderIpk?.toHex()?.let(senderNames::get))
            })
        if (!alertThisChat && rendered[convHex] == snapshot) return
        val silent = when (mode) {
            NotifBuzz.EveryMessage, NotifBuzz.FirstOnly -> !alertThisChat
            NotifBuzz.Throttled -> {
                val now = SystemClock.elapsedRealtime()
                val buzz = alertThisChat && now - (lastBuzz[convHex] ?: 0L) > BUZZ_THROTTLE_MS
                if (buzz) lastBuzz[convHex] = now
                !buzz
            }
        }

        val chat = NotificationCompat.Builder(app, Notifications.MESSAGES_CHANNEL)
            .setSmallIcon(R.drawable.i_logo_mono)
            .setColor(BRAND_COLOR)
            .setNumber(n) // unread count → OEM launcher/shade badge (the "2/3" pill)
            // The sender's send time, not now: a drain after hours offline would
            // otherwise stamp every queued message with the moment it arrived.
            .setWhen(newest * 1000)
            .setShowWhen(true)
            .setGroup(Notifications.GROUP_KEY)
            .setAutoCancel(true)
            .setSilent(silent)
            .setContentIntent(openChat(convHex, displayName)) // peer rides in the extras even when hidden
            .setCategory(NotificationCompat.CATEGORY_MESSAGE)
        if (ChatPrefs.notifPreview) {
            val avatar = letterAvatar(displayName) // no contact photos exist — colored initials, like the app
            val avatarIcon = IconCompat.createWithBitmap(avatar)
            chat.setLargeIcon(avatar)
            val them = Person.Builder().setName(displayName).setKey(convHex).setIcon(avatarIcon).build()

            // Promote into the system's Conversations section (prominent avatar, heads-up priority):
            // a long-lived dynamic shortcut carrying the Person, tied to the notif by id + LocusId.
            val shortcut = ShortcutInfoCompat.Builder(app, convHex)
                .setLongLived(true)
                .setShortLabel(displayName)
                .setPerson(them)
                .setIcon(avatarIcon)
                .setIntent(
                    Intent(app, LauncherActivity::class.java)
                        .setAction(Intent.ACTION_VIEW)
                        .putExtra(EXTRA_CONVERSATION, convHex)
                        .putExtra(EXTRA_CONV_NAME, displayName),
                )
                .build()
            ShortcutManagerCompat.pushDynamicShortcut(app, shortcut)
            chat.setShortcutInfo(shortcut).setLocusId(LocusIdCompat(convHex))

            val style = NotificationCompat.MessagingStyle(Person.Builder().setName("You").build())
                .setConversationTitle(displayName.takeIf { isGroup })
                .setGroupConversation(isGroup)
            recent.forEach { m ->
                val who = m.senderIpk?.toHex()?.let { senderNames[it] }
                val author = if (!isGroup || who == null) them
                             else Person.Builder().setName(who).setKey(who).build()
                val line = m.content.ifEmpty { mediaLabel(m.mediaKind.toInt()) }
                style.addMessage(line, m.timestamp.toLong() * 1000, author)
            }
            val replyPI = PendingIntent.getBroadcast(
                app, notifId(convHex),
                Intent(app, ReplyReceiver::class.java).putExtra("conversation", conv),
                PendingIntent.FLAG_MUTABLE or PendingIntent.FLAG_UPDATE_CURRENT, // MUTABLE: RemoteInput fills in the reply
            )
            val replyAction = NotificationCompat.Action.Builder(R.drawable.i_reply, "Reply", replyPI)
                .setSemanticAction(NotificationCompat.Action.SEMANTIC_ACTION_REPLY)
                .setShowsUserInterface(false)
                .addRemoteInput(RemoteInput.Builder(KEY_REPLY).setLabel("Reply").build())
                .build()
            chat.setStyle(style).addAction(replyAction)
        } else {
            // Preview off: no sender, no text, no Reply (nothing to reply to blind) — just a generic title.
            chat.setContentTitle("New message")
        }
        chat.addAction(readAction)
        if (mode == NotifBuzz.FirstOnly) chat.setOnlyAlertOnce(true) // only the first msg per live notif buzzes
        nm().notify(notifId(convHex), chat.build())
        rendered[convHex] = snapshot
        if (pending.isNotEmpty()) CoreBridge.markNotified(pending)

        nm().notify(
            SUMMARY_ID,
            NotificationCompat.Builder(app, Notifications.MESSAGES_CHANNEL)
                .setSmallIcon(R.drawable.i_logo_mono)
                .setColor(BRAND_COLOR)
                .setGroup(Notifications.GROUP_KEY)
                .setGroupSummary(true)
                .setAutoCancel(true)
                .setSilent(true) // the child is the only buzzer; a sounding summary would double-alert
                .build(),
        )
    }

    private fun nm() = app.getSystemService(NotificationManager::class.java)

    private fun openChat(convHex: String, name: String): PendingIntent {
        val intent = Intent(app, LauncherActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP)
            .putExtra(EXTRA_CONVERSATION, convHex)
            .putExtra(EXTRA_CONV_NAME, name)
        // Per-peer request code: extras aren't part of PendingIntent equality, so a shared code (0)
        // would alias every tap onto whichever chat's notification updated last.
        return PendingIntent.getActivity(
            app, notifId(convHex), intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
    }

    // ponytail: "You: …" reply-preview re-notify skipped — the reconcile after the send DB write
    // repaints the thread anyway; add a synchronous preview only if the ~150ms feels laggy.

    /** Deterministic per-peer id (so a cold FCM wake updates the same chat's notification instead of
     *  duplicating), kept clear of the reserved summary (1) / drain-FGS (42) ids. */
    private fun notifId(convHex: String): Int {
        val h = convHex.hashCode() and 0x7FFF_FFFF
        return if (h < RESERVED_MAX) h + RESERVED_MAX else h
    }

    /** Cancel this chat's notification straight away (reply/read receivers) so the RemoteInput
     *  spinner resolves without waiting on the debounced reconcile. Takes the receiver's context —
     *  the process may be cold, before [start] set [app]. */
    internal fun cancelChat(context: Context, convHex: String) =
        NotificationManagerCompat.from(context).cancel(notifId(convHex))

    /** Colored initials avatar — contacts carry no photo, so this mirrors the in-app letter avatar
     *  (a distinguishing hue per name beats the system's flat-gray fallback). */
    private fun letterAvatar(name: String, px: Int = 128): Bitmap {
        val initials = name.split(" ").filter { it.isNotBlank() }
            .take(2).joinToString("") { it.first().uppercase() }.ifEmpty { "?" }
        val bmp = Bitmap.createBitmap(px, px, Bitmap.Config.ARGB_8888)
        val canvas = Canvas(bmp)
        canvas.drawCircle(px / 2f, px / 2f, px / 2f, Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = AVATAR_COLORS[(name.hashCode() and 0x7FFF_FFFF) % AVATAR_COLORS.size]
        })
        val text = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = Color.WHITE
            textSize = px * 0.42f
            textAlign = Paint.Align.CENTER
            typeface = Typeface.create(Typeface.DEFAULT, Typeface.BOLD)
        }
        canvas.drawText(initials, px / 2f, px / 2f - (text.descent() + text.ascent()) / 2f, text)
        return bmp
    }

    private val AVATAR_COLORS = intArrayOf(
        0xFF0F66FF.toInt(), 0xFF00B2FF.toInt(), 0xFF7C4DFF.toInt(),
        0xFFEF5350.toInt(), 0xFF26A69A.toInt(), 0xFFFFA726.toInt(),
    )

    private val lastBuzz = mutableMapOf<String, Long>()

    private const val MAX_LINES = 8
    private const val RESERVED_MAX = 100
    private const val BUZZ_THROTTLE_MS = 2000L
    private val BRAND_COLOR = 0xFF00B2FF.toInt() // notification accent — tints the mono logo (the foreground)
}
