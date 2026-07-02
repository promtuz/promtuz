package com.promtuz.chat.ui.components

import android.content.Intent
import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import com.promtuz.chat.utils.common.parseMessageDate
import com.promtuz.chat.domain.model.Chat as ChatModel
import com.promtuz.chat.ui.activities.Chat as ChatActivity

@Composable
fun HomeChatListItem(chat: ChatModel, roundShape: Shape) {
    val context = LocalContext.current
    val textTheme = MaterialTheme.typography
    val colors = MaterialTheme.colorScheme

    val (_, name, msg) = chat

    Row(
        Modifier
            .fillMaxWidth()
            .clip(roundShape)
            .background(colors.surfaceContainer.copy(0.75f))
            .combinedClickable(onClick = {
                context.startActivity(Intent(context, ChatActivity::class.java).apply {
                    putExtra("user", chat.identity)
                    putExtra("name", chat.nickname)
                })
            }, onLongClick = {

            })
            .padding(vertical = 10.dp, horizontal = 12.dp),
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        verticalAlignment = Alignment.CenterVertically
    ) {
        Avatar(name)

        Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Row(
                Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.Top
            ) {
                Text(
                    name,
                    style = textTheme.titleMediumEmphasized,
                    color = colors.onSecondaryContainer
                )

                if (msg.timestamp > 0) {
                    Text(
                        parseMessageDate(msg.timestamp),
                        style = textTheme.bodySmallEmphasized,
                        color = colors.onSecondaryContainer.copy(0.5f)
                    )
                }
            }

            msg.content?.let {
                Text(
                    it,
                    style = textTheme.bodySmallEmphasized,
                    color = colors.onSecondaryContainer.copy(0.7f)
                )
            }
        }
    }
}
