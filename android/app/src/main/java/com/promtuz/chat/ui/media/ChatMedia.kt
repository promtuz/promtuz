package com.promtuz.chat.ui.media

import androidx.compose.ui.graphics.ImageBitmap
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.ui.components.MenuAction
import com.promtuz.chat.ui.components.isMediaTile
import com.promtuz.chat.ui.text.dateAndTime
import com.promtuz.chat.utils.media.decodeDownscaled
import android.content.Context
import android.net.Uri
import androidx.compose.ui.graphics.asImageBitmap
import java.io.File

/** Oldest first, so swiping right walks back in time; [messages] is newest first. */
fun chatMediaItems(
    context: Context,
    messages: List<UiMessage>,
    conversation: String,
    chatName: String,
    startKey: String,
    onDelete: (dispatchIdHex: String, outgoing: Boolean) -> Unit,
): Pair<List<MediaItem>, Int> {
    val items = ArrayList<MediaItem>()
    for (msg in messages.asReversed()) {
        if (msg.deleted) continue
        val did = msg.dispatchIdHex ?: continue
        val title = if (msg.outgoing) "You" else msg.senderName ?: chatName
        val subtitle = dateAndTime(context, msg.timestampMs)
        val location = com.promtuz.chat.domain.model.MessageLocation(conversation, chatName, did, msg.timestampMs)
        // Each item deletes itself: an album's photos are separate messages.
        fun delete(target: String) =
            listOf(listOf(MenuAction("Delete", R.drawable.oi_trash, destructive = true) { onDelete(target, msg.outgoing) }))
        when (val c = msg.content) {
            is MessageContent.Image -> items += MediaItem(
                key = did, thumb = c.bitmap, width = c.width, height = c.height,
                title = title, subtitle = subtitle, caption = c.caption, actions = delete(did), mime = "image/avif", message = location,
            )
            is MessageContent.Album -> c.items.forEachIndexed { i, item ->
                val image = item.content as? MessageContent.Image ?: return@forEachIndexed
                items += MediaItem(
                    key = item.dispatchIdHex, thumb = image.bitmap, width = image.width, height = image.height,
                    title = title, subtitle = subtitle, caption = if (i == 0) c.caption else "", actions = delete(item.dispatchIdHex),
                    group = did, mime = "image/avif", message = location,
                )
            }
            is MessageContent.Attachment -> {
                val path = c.localPath ?: continue
                if (!c.isMediaTile) continue
                val video = c.mime.startsWith("video/")
                items += MediaItem(
                    key = did, thumb = c.thumb, width = c.thumb?.width ?: 1, height = c.thumb?.height ?: 1,
                    title = title, subtitle = subtitle, caption = c.caption, actions = delete(did),
                    shareName = c.name, mime = c.mime, filePath = path, byteSize = c.size, message = location,
                    videoPath = if (video) path else null,
                    load = if (video) ({ c.thumb }) else ({ decodeDownscaled(context, Uri.fromFile(File(path)), 4096)?.asImageBitmap() ?: c.thumb }),
                )
            }
            else -> Unit
        }
    }
    return items to items.indexOfFirst { it.key == startKey }.coerceAtLeast(0)
}

fun pictureItem(key: String, image: ImageBitmap, title: String, actions: List<List<MenuAction>> = emptyList()) =
    MediaItem(key = key, thumb = image, width = image.width, height = image.height, title = title, actions = actions, mime = "image/avif")
