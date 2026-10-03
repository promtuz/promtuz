package com.promtuz.core.call

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.media.AudioAttributes
import android.provider.Settings
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.Person
import com.promtuz.chat.LauncherActivity
import com.promtuz.chat.R
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.core.push.PushNotifier

/** A ringing call is a CallStyle notification whose full-screen intent opens the call over the lock screen. */
object CallNotifications {
    const val CHANNEL = "calls"
    /** CallStyle is only the look; the ringtone comes from the channel. */
    private const val RING_CHANNEL = "calls_ring"
    const val ONGOING_ID = 71
    private const val MISSED_ID = 72

    fun ensureChannel(context: Context) {
        val nm = context.getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL, "Calls", NotificationManager.IMPORTANCE_HIGH).apply {
                setSound(null, null)
                enableVibration(true)
                setBypassDnd(true)
                lockscreenVisibility = Notification.VISIBILITY_PUBLIC
            },
        )
        nm.createNotificationChannel(
            NotificationChannel(RING_CHANNEL, "Incoming calls", NotificationManager.IMPORTANCE_HIGH).apply {
                setSound(
                    Settings.System.DEFAULT_RINGTONE_URI,
                    AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_NOTIFICATION_RINGTONE)
                        .setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
                        .build(),
                )
                enableVibration(true)
                setBypassDnd(true)
                lockscreenVisibility = Notification.VISIBILITY_PUBLIC
            },
        )
    }

    fun ringing(context: Context, ui: CallController.Ui) {
        post(context, ONGOING_ID, build(context, ui, ringing = true))
    }

    fun ongoing(context: Context, ui: CallController.Ui) {
        post(context, ONGOING_ID, build(context, ui, ringing = false))
    }

    fun clearOngoing(context: Context) {
        NotificationManagerCompat.from(context).cancel(ONGOING_ID)
    }

    /** A null [ui] gives a placeholder, since the service must promote with something. */
    fun build(context: Context, ui: CallController.Ui?, ringing: Boolean): Notification {
        ensureChannel(context)
        val name = ui?.name?.ifEmpty { "Call" } ?: "Call"
        val person = Person.Builder().setName(name).build()
        val incoming = ringing && ui?.outgoing == false
        val builder = NotificationCompat.Builder(context, if (incoming) RING_CHANNEL else CHANNEL)
            .setSmallIcon(R.drawable.i_phone)
            .setCategory(NotificationCompat.CATEGORY_CALL)
            .setOngoing(true)
            .setOnlyAlertOnce(!ringing)

        if (ui == null) {
            return builder.setContentTitle("Call").build()
        }

        val content = fullScreenIntent(context)
        if (incoming) {
            builder.setStyle(
                NotificationCompat.CallStyle.forIncomingCall(
                    person, hangup(context), answer(context, ui.callId),
                ),
            ).setFullScreenIntent(content, true)
                // Outlives core's 45 s ring, so a ring the process can no longer end still stops.
                .setTimeoutAfter(50_000)
            // Ring until answered or ended, not once.
            return builder.build().also { it.flags = it.flags or Notification.FLAG_INSISTENT }
        }
        builder.setStyle(NotificationCompat.CallStyle.forOngoingCall(person, hangup(context)))
            .setContentIntent(content)
        return builder.build()
    }

    /** Tagged by chat, so a second missed call from the same person replaces the first. */
    fun missed(context: Context, conversation: ByteArray, name: String) {
        ensureChannel(context)
        val who = name.ifEmpty { "Someone" }
        val open = Intent(context, LauncherActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP)
            .putExtra(PushNotifier.EXTRA_CONVERSATION, conversation.toHex())
            .putExtra(PushNotifier.EXTRA_CONV_NAME, name)
        val notif = NotificationCompat.Builder(context, CHANNEL)
            .setSmallIcon(R.drawable.i_phone)
            .setCategory(NotificationCompat.CATEGORY_MISSED_CALL)
            .setContentTitle("Missed call")
            .setContentText(who)
            .setAutoCancel(true)
            .setContentIntent(PendingIntent.getActivity(context, conversation.contentHashCode(), open, pendingFlags()))
            .build()
        runCatching { NotificationManagerCompat.from(context).notify(conversation.toHex(), MISSED_ID, notif) }
    }

    private fun fullScreenIntent(context: Context): PendingIntent {
        val intent = Intent(context, CallActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP)
        return PendingIntent.getActivity(context, 0, intent, pendingFlags())
    }

    /** Opens the call screen itself, which answers once it has the microphone. Android 12+
     *  blocks a receiver from starting an activity off a notification tap. */
    private fun answer(context: Context, call: ByteArray): PendingIntent {
        val intent = Intent(context, CallActivity::class.java)
            .setAction(CallActivity.ACTION_ANSWER)
            .putExtra(CallActivity.EXTRA_CALL, call)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP)
        // One PendingIntent per call, so the next call's extras never rewrite this one's.
        return PendingIntent.getActivity(context, call.contentHashCode(), intent, pendingFlags())
    }

    private fun hangup(context: Context): PendingIntent {
        val intent = Intent(context, CallActionReceiver::class.java).setAction(CallActionReceiver.ACTION_HANGUP)
        return PendingIntent.getBroadcast(context, 0, intent, pendingFlags())
    }

    private fun pendingFlags() = PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE

    private fun post(context: Context, id: Int, notif: Notification) {
        runCatching { NotificationManagerCompat.from(context).notify(id, notif) }
    }
}
