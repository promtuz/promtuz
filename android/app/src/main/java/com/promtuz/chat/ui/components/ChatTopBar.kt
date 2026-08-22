package com.promtuz.chat.ui.components

import android.text.format.DateUtils
import androidx.activity.compose.LocalOnBackPressedDispatcherOwner
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.presentation.viewmodel.ChatVM
import org.koin.compose.koinInject
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.appearance.chatBarHaze
import com.promtuz.chat.ui.util.freezeOnExit
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeEffect

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChatTopBar(name: String, chatVM: ChatVM, haze: HazeState) {
    val navigator = koinInject<AppVM>().navigator
    val backHandle = LocalOnBackPressedDispatcherOwner.current
    val colors = MaterialTheme.colorScheme
    val chatTheme = LocalChatColors.current
    val typing by chatVM.typing.collectAsState()
    val presence by chatVM.presence.collectAsState()
    val isGroup by chatVM.isGroup.collectAsState()
    val memberNames by chatVM.memberNames.collectAsState()
    val memberCount by chatVM.memberCount.collectAsState()
    val rawTitle by chatVM.rawTitle.collectAsState()
    val muted by chatVM.muted.collectAsState()
    val typingMembers by chatVM.typingMembers.collectAsState()


    // Who's typing, named — a group can have several at once, and "3 people
    // typing…" reads better than three names past a couple.
    val typingLine = remember(typingMembers, memberNames) {
        val names = typingMembers.mapNotNull { memberNames[it] }
        when {
            names.isEmpty() -> "typing…"
            names.size == 1 -> "${names[0]} is typing…"
            names.size == 2 -> "${names[0]} and ${names[1]} are typing…"
            else -> "${names.size} people are typing…"
        }
    }

    // Subtitle cascade: live activity beats presence; silence renders nothing.
    // A group has no single presence, so it falls back to its member count.
    val (subtitle, subtitleColor) = when {
        typing && isGroup -> typingLine to chatTheme.accent
        typing -> "typing…" to chatTheme.accent
        isGroup -> memberTally(memberCount) to colors.onSurfaceVariant
        presence == Presence.Online -> "online" to chatTheme.accent
        presence is Presence.Idle -> {
            val since = (presence as Presence.Idle).sinceMs
            val rel = DateUtils.getRelativeTimeSpanString(
                since,
                System.currentTimeMillis(),
                DateUtils.MINUTE_IN_MILLIS
            )
            "idle since $rel" to colors.onSurfaceVariant
        }

        presence is Presence.LastSeen -> {
            val at = (presence as Presence.LastSeen).atMs
            val rel = DateUtils.getRelativeTimeSpanString(
                at,
                System.currentTimeMillis(),
                DateUtils.MINUTE_IN_MILLIS
            )
            "last seen $rel" to colors.onSurfaceVariant
        }

        else -> null to colors.onSurfaceVariant
    }

    TopAppBar(
        title = {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                if (isGroup) GroupAvatar(title = rawTitle, members = memberNames.values.toList(), size = 40.dp)
                else Avatar(name, 40.dp)
                Column {
                    Text(name, style = MaterialTheme.typography.titleMediumEmphasized, maxLines = 1)
                    if (subtitle != null) Text(
                        subtitle,
                        style = MaterialTheme.typography.labelMedium,
                        color = subtitleColor,
                    )
                }
            }
        },
        navigationIcon = {
            Row(Modifier.fillMaxHeight()) {
                Spacer(Modifier.width(6.dp))
                DrawableIcon(
                    R.drawable.i_back_chevron, Modifier
                        .height(40.dp)
                        .align(
                            Alignment.CenterVertically
                        )
                        .clip(RoundedCornerShape(8.dp))
                        .clickable {
                            backHandle?.onBackPressedDispatcher?.onBackPressed()
                        })
            }
        },
        actions = {
            AppDropMenu(
                iconSize = 20.dp,
                anchor = { DrawableIcon(R.drawable.i_ellipsis_vertical, Modifier.padding(12.dp)) },
                groups = buildList {
                    if (isGroup) {
                        add(
                            listOf(
                                MenuAction("Group info", R.drawable.i_contacts) {
                                    navigator.push(Routes.GroupInfo(chatVM.conversationHex))
                                },
                            ),
                        )
                    }
                    add(
                        listOf(
                            MenuAction("Search", R.drawable.oi_search) {},
                            MenuAction(if (muted) "Unmute" else "Mute", if (muted) R.drawable.oi_bell_on else R.drawable.oi_bell_slash) {
                                chatVM.toggleMute()
                            })
                    )
                    add(
                        listOf(
                            MenuAction("Clear History", R.drawable.oi_clear_list) {},
                            MenuAction("Delete Chat", R.drawable.oi_trash, destructive = true) {},
                        ),
                    )
                },
            )
        },
        // freezeOnExit: bake the blur to pixels while the nav card scales out (Haze
        // samples screen-space and shatters under an ancestor scale).
        modifier = Modifier
            .freezeOnExit()
            .hazeEffect(haze, chatBarHaze()),
        colors = TopAppBarDefaults.topAppBarColors(containerColor = Color.Transparent),
    )
}

/** "1 member" / "4 members" — a group of one is a real state after a removal. */
fun memberTally(n: Int): String = if (n == 1) "1 member" else "$n members"
