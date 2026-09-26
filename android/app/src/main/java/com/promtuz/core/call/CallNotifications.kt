package com.promtuz.core.call

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.Person
import com.promtuz.chat.R
import com.promtuz.chat.utils.extensions.toHex

/**
 * The call's presence in the shade and on the lock screen. A ringing call is a
 * CallStyle notification with a full-screen intent, so the incoming screen
 * comes up over the lock screen the way a phone call does; androidx backports
 * the style to older versions. No Telecom, on purpose.
 */
object CallNotifications {
    const val CHANNEL = "calls"
    const val ONGOING_ID = 71
    private const val MISSED_ID = 72

    fun ensureChannel(context: Context) {
        val nm = context.getSystemService(NotificationManager::class.java)
        val channel = NotificationChannel(CHANNEL, "Calls", NotificationManager.IMPORTANCE_HIGH).apply {
            setSound(null, null) // the CallStyle notification rings itself
            enableVibration(true)
            setBypassDnd(true)
            lockscreenVisibility = Notification.VISIBILITY_PUBLIC
        }
        nm.createNotificationChannel(channel)
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

    /** Build the ongoing/ringing notification, or a minimal placeholder when
     *  state is momentarily null (the service promotes with something). */
    fun build(context: Context, ui: CallController.Ui?, ringing: Boolean): Notification {
        ensureChannel(context)
        val name = ui?.name?.ifEmpty { "Call" } ?: "Call"
        val person = Person.Builder().setName(name).build()
        val builder = NotificationCompat.Builder(context, CHANNEL)
            .setSmallIcon(R.drawable.i_phone)
            .setCategory(NotificationCompat.CATEGORY_CALL)
            .setOngoing(true)
            .setOnlyAlertOnce(!ringing)

        if (ui == null) {
            return builder.setContentTitle("Call").build()
        }

        val content = fullScreenIntent(context)
        if (ringing && !ui.outgoing) {
            builder.setStyle(
                NotificationCompat.CallStyle.forIncomingCall(
                    person, hangup(context), answer(context),
                ),
            ).setFullScreenIntent(content, true)
        } else {
            builder.setStyle(NotificationCompat.CallStyle.forOngoingCall(person, hangup(context)))
                .setContentIntent(content)
        }
        return builder.build()
    }

    fun missed(context: Context, call: ByteArray, conversation: ByteArray, name: String) {
        ensureChannel(context)
        val who = name.ifEmpty { "Someone" }
        val notif = NotificationCompat.Builder(context, CHANNEL)
            .setSmallIcon(R.drawable.i_phone)
            .setCategory(NotificationCompat.CATEGORY_MISSED_CALL)
            .setContentTitle("Missed call")
            .setContentText(who)
            .setAutoCancel(true)
            .setContentIntent(openChat(context, conversation, who))
            .build()
        post(context, MISSED_ID + (call.firstOrNull()?.toInt() ?: 0), notif)
    }

    /** Open the conversation (not the call screen, which self-finishes when no
     *  call is live) so a tapped missed call lands in its chat. */
    private fun openChat(context: Context, conversation: ByteArray, name: String): PendingIntent {
        val intent = Intent(context, com.promtuz.chat.LauncherActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP)
            .putExtra(com.promtuz.core.push.PushNotifier.EXTRA_CONVERSATION, conversation.toHex())
            .putExtra(com.promtuz.core.push.PushNotifier.EXTRA_CONV_NAME, name)
        return PendingIntent.getActivity(
            context, conversation.contentHashCode(), intent, pendingFlags(),
        )
    }

    private fun fullScreenIntent(context: Context): PendingIntent {
        val intent = Intent(context, CallActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP)
        return PendingIntent.getActivity(context, 0, intent, pendingFlags())
    }

    private fun answer(context: Context) =
        action(context, CallActionReceiver.ACTION_ANSWER, "Answer")

    private fun hangup(context: Context) =
        action(context, CallActionReceiver.ACTION_HANGUP, "Hang up")

    private fun action(context: Context, action: String, title: String): PendingIntent {
        val intent = Intent(context, CallActionReceiver::class.java).setAction(action)
        return PendingIntent.getBroadcast(context, action.hashCode(), intent, pendingFlags())
    }

    private fun pendingFlags() = PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE

    private fun post(context: Context, id: Int, notif: Notification) {
        // POST_NOTIFICATIONS is requested at onboarding; a refusal only loses
        // the shade entry, the full-screen intent still fires.
        runCatching { NotificationManagerCompat.from(context).notify(id, notif) }
    }
}
