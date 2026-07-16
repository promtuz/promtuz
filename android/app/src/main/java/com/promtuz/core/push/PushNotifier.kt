package com.promtuz.core.push

import android.Manifest
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
import androidx.core.app.ActivityCompat
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.Person
import androidx.core.app.RemoteInput
import androidx.core.content.LocusIdCompat
import androidx.core.content.pm.ShortcutInfoCompat
import androidx.core.content.pm.ShortcutManagerCompat
import androidx.core.graphics.drawable.IconCompat
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import com.promtuz.chat.LauncherActivity
import com.promtuz.chat.R
import com.promtuz.core.CoreBridge
import com.promtuz.core.adapter.CoreEventBus
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.data.NotifBuzz
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.toHex
import java.util.concurrent.ConcurrentHashMap
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.debounce
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.launch

/**
 * Turns unread incoming messages into notifications — one
 * [NotificationCompat.MessagingStyle] per chat, grouped under a summary. Nothing
 * here decrypts or trusts the wake payload; it re-reads the DB.
 *
 * Reconcile model: no per-event patching. Every DB change RECONCILES the whole
 * notification set from [CoreBridge.unreadCounts] — a read chat's notif clears
 * itself (from any surface), and only a genuine new inbound buzzes.
 */
object PushNotifier {
    private const val SUMMARY_ID = 1

    /** RemoteInput result key; shared with [ReplyReceiver]. */
    const val KEY_REPLY = "reply_text"

    /** Notification-tap extras: which chat to open (hex IPK + display name). */
    const val EXTRA_PEER = "chat_peer_hex"
    const val EXTRA_PEER_NAME = "chat_peer_name"

    private lateinit var app: Application
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var names: Map<String, String> = emptyMap()

    /** Gates posting — reconcile still runs foregrounded, but only to clear read chats' notifs. */
    @Volatile
    private var foreground = false

    fun start(application: Application) {
        app = application
        Notifications.ensureChannels(application)
        ProcessLifecycleOwner.get().lifecycle.addObserver(object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) { foreground = true }
            override fun onStop(owner: LifecycleOwner) {
                foreground = false
                scope.launch { reconcile(alertPeer = null) } // paint the current unread set on background
            }
        })
        // A genuine new inbound: reconcile and buzz that peer — only when we're not already looking.
        scope.launch {
            CoreEventBus.incoming.collect { msg -> if (!foreground) reconcile(alertPeer = msg.peerHex) }
        }
        // Any other message write (edit/delete/local-read): silent reconcile — repaint/clear only.
        scope.launch {
            CoreEventBus.dbChanged.filter { "messages" in it }.debounce(150L).collect { reconcile(alertPeer = null) }
        }
    }

    private suspend fun reconcile(alertPeer: String?) {
        if (!::app.isInitialized) return
        if (ActivityCompat.checkSelfPermission(app, Manifest.permission.POST_NOTIFICATIONS)
            != PackageManager.PERMISSION_GRANTED
        ) return

        val nm = app.getSystemService(NotificationManager::class.java)

        // Master off: nuke our whole group (children + summary) and post nothing — flipping the switch
        // off should silence AND clear the shade, not just stop future buzzes.
        if (!ChatPrefs.notifEnabled) {
            nm.activeNotifications
                .filter { it.notification.group == Notifications.GROUP_KEY }
                .forEach { nm.cancel(it.id) }
            return
        }

        val counts = runCatching { CoreBridge.unreadCounts() }.getOrDefault(emptyList())
            .associate { it.peerIpk.toHex() to it.count.toInt() }

        // Muted chats drop out entirely: they neither post nor stay in `live`, so muting a chat also
        // clears any notif it already had.
        val muted = ChatPrefs.muted.value
        val visible = counts.filterKeys { it !in muted }

        // Dismiss per-chat notifs whose chat is no longer unread (read from any surface) or now muted.
        // Unconditional (runs foregrounded too), so an in-app read or a mute clears the shade. GROUP_KEY
        // filter spares the drain worker's foreground-service notice (SYNC id 42, no group).
        val live = visible.keys.map(::notifId).toSet()
        nm.activeNotifications
            .filter { it.notification.group == Notifications.GROUP_KEY && it.id != SUMMARY_ID && it.id !in live }
            .forEach { nm.cancel(it.id) }

        if (visible.isEmpty()) {
            nm.cancel(SUMMARY_ID)
            return
        }

        if (visible.keys.any { it !in names }) {
            names = runCatching { CoreBridge.contacts().associate { it.ipk.toHex() to it.name } }
                .getOrDefault(names)
        }
        // Foregrounded: clear-only, no new shade notifs while you're already looking at the app.
        // ponytail: no 7-dialog cap (limit concurrent chat notifs) — add if noisy.
        if (!foreground) visible.forEach { (peerHex, n) -> postChat(peerHex, n, alertPeer) }
    }

    private suspend fun postChat(peerHex: String, n: Int, alertPeer: String?) {
        val peer = peerHex.fromHex()
        val displayName = names[peerHex] ?: "New message"

        // takeLast(n) of incoming ≈ the unread ones (read is a high-water-mark, so the
        // newest n incoming are exactly the unread), hydrated from the DB (survives process death).
        val recent = runCatching { CoreBridge.messages(peer, MAX_LINES) }
            .getOrDefault(emptyList())
            .filter { !it.deleted && !it.outgoing }
            .sortedBy { it.timestamp }
            .takeLast(n)
        // All newest-window rows deleted-for-everyone while count>0: nothing to paint, so clear this
        // chat's own notif (a bare return would strand a stale one reconcile can't repaint or cancel).
        if (recent.isEmpty()) { nm().cancel(notifId(peerHex)); return }

        val readPI = PendingIntent.getBroadcast(
            app, -notifId(peerHex),
            Intent(app, MarkReadReceiver::class.java).putExtra("peer", peer),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val readAction = NotificationCompat.Action.Builder(R.drawable.i_check, "Mark read", readPI)
            .setSemanticAction(NotificationCompat.Action.SEMANTIC_ACTION_MARK_AS_READ)
            .setShowsUserInterface(false)
            .build()

        // Buzz only the peer this reconcile fired for, and only per the configured cadence. Because the
        // alert rides the arriving peer (not a global flag), a silent reconcile racing in first can't
        // swallow it.
        val mode = ChatPrefs.notifBuzz
        val alertThisChat = peerHex == alertPeer
        val silent = when (mode) {
            NotifBuzz.EveryMessage, NotifBuzz.FirstOnly -> !alertThisChat
            NotifBuzz.Throttled -> {
                val now = System.currentTimeMillis()
                val buzz = alertThisChat && now - (lastBuzz[peerHex] ?: 0L) > BUZZ_THROTTLE_MS
                if (buzz) lastBuzz[peerHex] = now
                !buzz
            }
        }

        val chat = NotificationCompat.Builder(app, Notifications.MESSAGES_CHANNEL)
            .setSmallIcon(R.drawable.i_logo_mono)
            .setColor(BRAND_COLOR)
            .setNumber(n) // unread count → OEM launcher/shade badge (the "2/3" pill)
            .setGroup(Notifications.GROUP_KEY)
            .setAutoCancel(true)
            .setSilent(silent)
            .setContentIntent(openChat(peerHex, displayName)) // peer rides in the extras even when hidden
            .setCategory(NotificationCompat.CATEGORY_MESSAGE)
        if (ChatPrefs.notifPreview) {
            val avatar = letterAvatar(displayName) // no contact photos exist — colored initials, like the app
            val avatarIcon = IconCompat.createWithBitmap(avatar)
            chat.setLargeIcon(avatar)
            val them = Person.Builder().setName(displayName).setKey(peerHex).setIcon(avatarIcon).build()

            // Promote into the system's Conversations section (prominent avatar, heads-up priority):
            // a long-lived dynamic shortcut carrying the Person, tied to the notif by id + LocusId.
            val shortcut = ShortcutInfoCompat.Builder(app, peerHex)
                .setLongLived(true)
                .setShortLabel(displayName)
                .setPerson(them)
                .setIcon(avatarIcon)
                .setIntent(
                    Intent(app, LauncherActivity::class.java)
                        .setAction(Intent.ACTION_VIEW)
                        .putExtra(EXTRA_PEER, peerHex)
                        .putExtra(EXTRA_PEER_NAME, displayName),
                )
                .build()
            ShortcutManagerCompat.pushDynamicShortcut(app, shortcut)
            chat.setShortcutInfo(shortcut).setLocusId(LocusIdCompat(peerHex))

            val style = NotificationCompat.MessagingStyle(Person.Builder().setName("You").build())
            recent.forEach { style.addMessage(it.content, it.timestamp.toLong() * 1000, them) }
            val replyPI = PendingIntent.getBroadcast(
                app, notifId(peerHex),
                Intent(app, ReplyReceiver::class.java).putExtra("peer", peer),
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
        nm().notify(notifId(peerHex), chat.build())

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

    private fun openChat(peerHex: String, name: String): PendingIntent {
        val intent = Intent(app, LauncherActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP)
            .putExtra(EXTRA_PEER, peerHex)
            .putExtra(EXTRA_PEER_NAME, name)
        // Per-peer request code: extras aren't part of PendingIntent equality, so a shared code (0)
        // would alias every tap onto whichever chat's notification updated last.
        return PendingIntent.getActivity(
            app, notifId(peerHex), intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
    }

    // ponytail: "You: …" reply-preview re-notify skipped — the reconcile after the send DB write
    // repaints the thread anyway; add a synchronous preview only if the ~150ms feels laggy.

    /** Deterministic per-peer id (so a cold FCM wake updates the same chat's notification instead of
     *  duplicating), kept clear of the reserved summary (1) / drain-FGS (42) ids. */
    private fun notifId(peerHex: String): Int {
        val h = peerHex.hashCode() and 0x7FFF_FFFF
        return if (h < RESERVED_MAX) h + RESERVED_MAX else h
    }

    /** Cancel this chat's notification straight away (reply/read receivers) so the RemoteInput
     *  spinner resolves without waiting on the debounced reconcile. Takes the receiver's context —
     *  the process may be cold, before [start] set [app]. */
    internal fun cancelChat(context: Context, peerHex: String) =
        NotificationManagerCompat.from(context).cancel(notifId(peerHex))

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

    /** Per-peer last-buzz wall-clock for [NotifBuzz.Throttled]. ConcurrentHashMap: the incoming and
     *  dbChanged collectors can reconcile on different IO threads at once. */
    private val lastBuzz = ConcurrentHashMap<String, Long>()

    private const val MAX_LINES = 8
    private const val RESERVED_MAX = 100
    private const val BUZZ_THROTTLE_MS = 2000L
    private val BRAND_COLOR = 0xFF00B2FF.toInt() // notification accent — tints the mono logo (the foreground)
}
