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
import android.graphics.BitmapShader
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Matrix
import android.graphics.Paint
import android.graphics.Shader
import android.graphics.Typeface
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.graphics.toArgb
import android.os.Bundle
import android.os.SystemClock
import android.net.Uri
import androidx.core.content.FileProvider
import com.promtuz.chat.utils.media.MessagePreviews
import java.io.ByteArrayOutputStream
import java.io.File
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.coroutineScope
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
import uniffi.core.ConversationRecord

/** Reconciles the notification shade with unread messages and persisted alert state. */
@OptIn(kotlinx.coroutines.FlowPreview::class)
object PushNotifier {
    private const val SUMMARY_ID = 1
    private const val REQUESTS_ID = 2
    @Volatile private var requestsVisible = false

    fun viewingRequests(visible: Boolean) { requestsVisible = visible; scope.launch { reconcileSafely() } }

    const val KEY_REPLY = "reply_text"

    const val EXTRA_CONVERSATION = "chat_conversation_hex"
    const val EXTRA_CONV_NAME = "chat_conversation_name"

    private lateinit var app: Application
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var names: Map<String, String> = emptyMap()

    private var senderNames: Map<String, String> = emptyMap()

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
        val image: Uri?,
    )

    @Volatile
    private var foreground = false

    @Volatile private var visibleConversation: String? = null

    fun viewing(conversation: String, visible: Boolean) {
        if (visible) visibleConversation = conversation
        else if (visibleConversation == conversation) visibleConversation = null
        scope.launch { reconcileSafely() }
    }

    private fun viewing(conversation: String): Boolean = foreground && visibleConversation == conversation

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
            CoreEventBus.dbChanged.filter { "messages" in it || "contacts" in it || "prefs" in it || "message_media" in it }
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

        val enabled = ChatPrefs.notifEnabled
        if (!enabled) {
            nm.activeNotifications
                .filter { it.notification.group == Notifications.GROUP_KEY }
                .forEach { nm.cancel(it.id) }
            rendered.clear()
            prunePreviewFiles(emptySet())
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
            if (!enabled || viewing(conv) || conv in mutedConvs) {
                val pending = CoreBridge.pendingNotificationIds(conv.fromHex())
                if (pending.isNotEmpty() && (!enabled || viewing(conv) || conv in mutedConvs)) {
                    CoreBridge.markNotified(pending)
                }
            }
        }
        val requests = convs.filter { it.request && (counts[it.id.toHex()] ?: 0) > 0 }
        reconcileRequests(enabled, requests)

        if (!enabled) return

        val requestConvs = requests.map { it.id.toHex() }.toSet()
        val visible = counts.filterKeys { it !in mutedConvs && !viewing(it) && it !in requestConvs }

        // The GROUP_KEY filter spares the drain worker's foreground-service notice, which has no group.
        val live = visible.keys.map(::notifId).toSet()
        nm.activeNotifications
            .filter { it.notification.group == Notifications.GROUP_KEY && it.id != SUMMARY_ID && it.id !in live }
            .forEach { nm.cancel(it.id) }
        rendered.keys.retainAll(visible.keys)
        prunePreviewFiles(visible.keys)

        if (visible.isEmpty()) {
            nm.cancel(SUMMARY_ID)
            return
        }

        for ((convHex, n) in visible) postChat(convHex, n)
    }

    /** One notification for all requests, with no Reply action: replying would accept the request. */
    private suspend fun reconcileRequests(enabled: Boolean, requests: List<ConversationRecord>) {
        val fresh = requests.flatMap { CoreBridge.pendingNotificationIds(it.id) }
        if (!enabled || requests.isEmpty() || requestsVisible) {
            nm().cancel(REQUESTS_ID)
            if (fresh.isNotEmpty()) CoreBridge.markNotified(fresh)
            return
        }
        if (fresh.isEmpty() && nm().activeNotifications.none { it.id == REQUESTS_ID }) return
        val intent = Intent(app, LauncherActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP)
            .putExtra("open_message_requests", true)
        val open = PendingIntent.getActivity(app, REQUESTS_ID, intent, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        val text = when {
            !ChatPrefs.notifPreview -> "You have a new message request"
            requests.size == 1 -> "${requests.first().displayName} sent you a message"
            else -> "${requests.size} people sent you messages"
        }
        val notification = NotificationCompat.Builder(app, Notifications.REQUESTS_CHANNEL)
            .setSmallIcon(R.drawable.i_logo_mono).setContentTitle("Message request")
            .setContentText(text).setContentIntent(open).setAutoCancel(true)
            .setSilent(fresh.isEmpty()).build()
        nm().notify(REQUESTS_ID, notification)
        if (fresh.isNotEmpty()) CoreBridge.markNotified(fresh)
    }

    private suspend fun postChat(convHex: String, n: Int) {
        val conv = convHex.fromHex()
        val displayName = names[convHex] ?: "New message"

        // The newest n incoming are the unread ones, since read is a high-water mark.
        val pending = CoreBridge.pendingNotificationIds(conv)
        val recent = CoreBridge.recentIncoming(conv, MAX_LINES).takeLast(n)
        if (viewing(convHex)) return
        // Everything unread was deleted for everyone. A bare return would strand a stale notification.
        if (recent.isEmpty()) { nm().cancel(notifId(convHex)); return }

        val readPI = PendingIntent.getBroadcast(
            app, -notifId(convHex),
            Intent(app, MarkReadReceiver::class.java).putExtra("conversation", conv),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val readAction = NotificationCompat.Action.Builder(R.drawable.oi_check, "Mark read", readPI)
            .setSemanticAction(NotificationCompat.Action.SEMANTIC_ACTION_MARK_AS_READ)
            .setShowsUserInterface(false)
            .build()

        val isGroup = groupConvs.contains(convHex)
        val authors = if (isGroup) senderNames + CoreBridge.members(conv).associate { it.ipk.toHex() to it.name }
                      else senderNames
        val newest = recent.last().timestamp.toLong()
        val mode = ChatPrefs.notifBuzz
        val alertThisChat = pending.isNotEmpty()
        val previewEnabled = ChatPrefs.notifPreview
        val images = if (previewEnabled) coroutineScope {
            recent.map { message -> async {
                val did = message.dispatchId.toHex()
                message.id to if (message.mediaKind.toInt() in listOf(1, 2, 4))
                    notificationImage(convHex, did) else null
            } }.awaitAll().toMap()
        } else emptyMap()
        // A read, mute or privacy change may have happened while a sticker was fetched.
        if (viewing(convHex) || !ChatPrefs.notifEnabled || ChatPrefs.notifPreview != previewEnabled ||
            CoreBridge.conversation(conv)?.muted == true ||
            CoreBridge.unreadCounts().none { it.conversationId.contentEquals(conv) && it.count.toInt() == n }) return
        val current = CoreBridge.recentIncoming(conv, MAX_LINES).takeLast(n)
        if (current.map { Triple(it.id, it.content, it.mediaKind) } !=
            recent.map { Triple(it.id, it.content, it.mediaKind) }) return
        val snapshot = ChatSnapshot(displayName, n, isGroup, ChatPrefs.notifPreview, mode,
            recent.map { message ->
                LineSnapshot(message.id, message.content, message.mediaKind.toInt(), message.timestamp,
                    message.senderIpk?.toHex()?.let(authors::get), images[message.id])
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
            .setNumber(n)
            // The send time, so a drain after hours offline does not stamp everything with its arrival.
            .setWhen(newest * 1000)
            .setShowWhen(true)
            .setGroup(Notifications.GROUP_KEY)
            .setAutoCancel(true)
            .setSilent(silent)
            .setContentIntent(openChat(convHex, displayName))
            .setCategory(NotificationCompat.CATEGORY_MESSAGE)
        if (ChatPrefs.notifPreview) {
            val identity = CoreBridge.conversation(conv)?.peer?.toHex() ?: convHex
            val avatar = notificationAvatar(if (isGroup) "group:$convHex" else identity)
                ?: letterAvatar(displayName, identity)
            val avatarIcon = IconCompat.createWithBitmap(avatar)
            chat.setLargeIcon(avatar)
            val them = Person.Builder().setName(displayName).setKey(convHex).setIcon(avatarIcon).build()

            // A long-lived shortcut carrying the Person puts this in the system's Conversations section.
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
                val sender = m.senderIpk?.toHex()
                val who = sender?.let { authors[it] ?: it.take(8) }
                val author = if (!isGroup || sender == null) them
                             else Person.Builder().setName(who).setKey(sender).apply {
                                 setIcon(IconCompat.createWithBitmap(notificationAvatar(sender)
                                     ?: letterAvatar(who.orEmpty(), sender)))
                             }.build()
                val line = m.content.ifEmpty { mediaLabel(m.mediaKind.toInt()) }
                val message = NotificationCompat.MessagingStyle.Message(line, m.timestamp.toLong() * 1000, author)
                images[m.id]?.let { message.setData("image/png", it) }
                style.addMessage(message)
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
            chat.setContentTitle("New message")
        }
        chat.addAction(readAction)
        if (mode == NotifBuzz.FirstOnly) chat.setOnlyAlertOnce(true)
        nm().notify(notifId(convHex), chat.build())
        rendered[convHex] = snapshot
        // Only disposable, generated previews; never the authoritative media files.
        val keep = images.values.filterNotNull().map { it.lastPathSegment }.toSet()
        File(app.cacheDir, "notification-previews").listFiles()?.filter {
            it.name.startsWith("$convHex-") && it.name !in keep
        }?.forEach { it.delete() }
        if (pending.isNotEmpty()) CoreBridge.markNotified(pending)

        nm().notify(
            SUMMARY_ID,
            NotificationCompat.Builder(app, Notifications.MESSAGES_CHANNEL)
                .setSmallIcon(R.drawable.i_logo_mono)
                .setColor(BRAND_COLOR)
                .setGroup(Notifications.GROUP_KEY)
                .setGroupSummary(true)
                .setAutoCancel(true)
                .setSilent(true) // a sounding summary would double-alert
                .build(),
        )
    }

    private fun nm() = app.getSystemService(NotificationManager::class.java)

    private fun prunePreviewFiles(conversations: Set<String>) {
        File(app.cacheDir, "notification-previews").listFiles()?.filter {
            it.name.substringBefore('-') !in conversations
        }?.forEach { it.delete() }
    }

    private suspend fun notificationImage(conversation: String, dispatch: String): Uri? {
        try {
            val bitmap = MessagePreviews.load(conversation, dispatch) ?: return null
            val bytes = ByteArrayOutputStream().use { out ->
                if (!bitmap.compress(Bitmap.CompressFormat.PNG, 100, out)) return null
                out.toByteArray()
            }
            val dir = File(app.cacheDir, "notification-previews").apply { mkdirs() }
            val file = File(dir, "$conversation-$dispatch-${bytes.contentHashCode()}.png")
            if (!file.exists()) {
                val pending = File.createTempFile("preview-", ".tmp", dir)
                try {
                    pending.writeBytes(bytes)
                    if (!pending.renameTo(file)) return null
                } finally { pending.delete() }
            }
            // MessagingStyle URIs are carried by the notification so Android grants
            // its listeners read access for the lifetime of that notification.
            return FileProvider.getUriForFile(app, "${app.packageName}.fileprovider", file)
        } catch (e: CancellationException) { throw e }
        catch (e: Exception) {
            Timber.tag("Push").d(e, "Message image preview unavailable")
            return null
        }
    }

    private fun openChat(convHex: String, name: String): PendingIntent {
        val intent = Intent(app, LauncherActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP)
            .putExtra(EXTRA_CONVERSATION, convHex)
            .putExtra(EXTRA_CONV_NAME, name)
        // Extras are not part of PendingIntent equality, so each chat needs its own request code.
        return PendingIntent.getActivity(
            app, notifId(convHex), intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
    }

    /** Stable per chat, and clear of the summary (1), requests (2) and drain-FGS (42) ids. */
    private fun notifId(convHex: String): Int {
        val h = convHex.hashCode() and 0x7FFF_FFFF
        return if (h < RESERVED_MAX) h + RESERVED_MAX else h
    }

    /** Cancels at once, so the RemoteInput spinner resolves without waiting for the debounced reconcile. */
    internal fun cancelChat(context: Context, convHex: String) =
        NotificationManagerCompat.from(context).cancel(notifId(convHex))

    private suspend fun notificationAvatar(key: String, px: Int = 128): Bitmap? {
        val src = com.promtuz.chat.utils.media.AvatarImages.load(key)?.asAndroidBitmap() ?: return null
        val out = Bitmap.createBitmap(px, px, Bitmap.Config.ARGB_8888)
        val scale = maxOf(px.toFloat() / src.width, px.toFloat() / src.height)
        val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            isFilterBitmap = true
            shader = BitmapShader(src, Shader.TileMode.CLAMP, Shader.TileMode.CLAMP).apply {
                setLocalMatrix(Matrix().apply {
                    setScale(scale, scale)
                    postTranslate((px - src.width * scale) / 2f, (px - src.height * scale) / 2f)
                })
            }
        }
        Canvas(out).drawCircle(px / 2f, px / 2f, px / 2f, paint)
        return out
    }

    private fun letterAvatar(name: String, identity: String, px: Int = 128): Bitmap {
        val initials = name.split(" ").filter { it.isNotBlank() }
            .take(2).joinToString("") { it.first().uppercase() }.ifEmpty { "?" }
        val bmp = Bitmap.createBitmap(px, px, Bitmap.Config.ARGB_8888)
        val canvas = Canvas(bmp)
        canvas.drawCircle(px / 2f, px / 2f, px / 2f, Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = com.promtuz.chat.ui.components.avatarColor(identity).toArgb()
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

    private val lastBuzz = mutableMapOf<String, Long>()

    private const val MAX_LINES = 8
    private const val RESERVED_MAX = 100
    private const val BUZZ_THROTTLE_MS = 2000L
    private val BRAND_COLOR = 0xFF00B2FF.toInt()
}
