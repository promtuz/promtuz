package com.promtuz.chat.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.promtuz.chat.presentation.viewmodel.GroupVM
import com.promtuz.chat.presentation.viewmodel.GroupWork
import com.promtuz.chat.presentation.viewmodel.UiMember
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.components.Avatar
import com.promtuz.chat.ui.components.GroupAvatar
import com.promtuz.chat.ui.components.memberTally
import org.koin.androidx.compose.koinViewModel

/**
 * A group's roster, with the management actions the local user is allowed.
 *
 * Departed members stay listed, greyed — their old messages still need a name
 * to attribute to, and hiding them would make the history read as anonymous.
 */
@Composable
fun GroupInfoScreen(conversationHex: String, viewModel: GroupVM = koinViewModel()) {
    val colors = MaterialTheme.colorScheme
    val chat = LocalChatColors.current
    val members by viewModel.members.collectAsStateWithLifecycle()
    val title by viewModel.groupTitle.collectAsStateWithLifecycle()
    val displayName by viewModel.displayName.collectAsStateWithLifecycle()
    val canManage by viewModel.canManage.collectAsStateWithLifecycle()
    val candidates by viewModel.candidates.collectAsStateWithLifecycle()
    val work by viewModel.work.collectAsStateWithLifecycle()
    val canLeave by viewModel.canLeave.collectAsStateWithLifecycle()
    val ownerIsStuck by viewModel.ownerIsStuck.collectAsStateWithLifecycle()

    LaunchedEffect(conversationHex) { viewModel.load(conversationHex) }

    var draftTitle by remember(title) { mutableStateOf(title) }
    var adding by remember { mutableStateOf(false) }

    val active = members.filter { it.active }
    // Anyone in the address book who isn't already here.
    val addable = remember(candidates, members) {
        val present = members.filter { it.active }.map { it.ipkHex }.toSet()
        candidates.filter { it.ipkHex !in present }
    }

    LazyColumn(Modifier.fillMaxSize()) {
        item {
            Column(
                Modifier.fillMaxWidth().padding(24.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                // Untitled groups fall back to members' initials — other
                // people's, not ours; our own face says nothing about which
                // group this is.
                GroupAvatar(
                    title = title,
                    members = active.filterNot { it.me }.map { it.name },
                    size = 84.dp,
                )
                Spacer(Modifier.height(12.dp))
                if (canManage) {
                    BasicTextField(
                        value = draftTitle,
                        onValueChange = { draftTitle = it },
                        singleLine = true,
                        textStyle = MaterialTheme.typography.titleLarge.copy(color = colors.onSurface),
                        cursorBrush = SolidColor(chat.accent),
                        modifier = Modifier.padding(bottom = 2.dp),
                    )
                    if (draftTitle.trim() != title) {
                        Text(
                            "Save name",
                            Modifier.clickable { viewModel.rename(draftTitle) }.padding(4.dp),
                            style = MaterialTheme.typography.labelMedium,
                            color = chat.accent,
                        )
                    }
                } else {
                    Text(
                        displayName,
                        style = MaterialTheme.typography.titleLarge,
                        color = colors.onSurface,
                    )
                }
                Text(
                    memberTally(active.size),
                    style = MaterialTheme.typography.bodySmall,
                    color = colors.onSurfaceVariant,
                )
            }
        }

        (work as? GroupWork.Failed)?.let {
            item {
                Text(
                    it.reason,
                    Modifier.fillMaxWidth().padding(horizontal = 20.dp, vertical = 8.dp),
                    style = MaterialTheme.typography.bodySmall,
                    color = colors.error,
                )
            }
        }

        if (canManage) {
            item {
                Row(
                    Modifier
                        .fillMaxWidth()
                        .clickable { adding = !adding }
                        .padding(horizontal = 20.dp, vertical = 12.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        if (adding) "Done adding" else "Add members",
                        style = MaterialTheme.typography.titleSmall,
                        fontWeight = FontWeight.SemiBold,
                        color = chat.accent,
                    )
                }
            }
            if (adding) {
                items(addable, key = { "add:${it.ipkHex}" }) { c ->
                    MemberRow(
                        member = c,
                        trailing = "Add",
                        trailingColor = chat.accent,
                        onTrailing = { viewModel.addMember(c.ipkHex) },
                    )
                }
            }
        }

        items(members, key = { it.ipkHex }) { m ->
            // Leaving is its own action at the bottom; never offer it as a
            // "Remove" on our own row.
            val removable = m.active && canManage && !m.admin && !m.me
            MemberRow(
                member = m,
                trailing = when {
                    !m.active -> "Past member"
                    m.admin -> "Admin"
                    removable -> "Remove"
                    else -> null
                },
                trailingColor = if (removable) colors.error else colors.onSurfaceVariant,
                onTrailing = if (removable) {
                    { viewModel.removeMember(m.ipkHex) }
                } else null,
            )
        }

        item {
            // The founder can't walk out on a group other people are still in,
            // so say why rather than offering an action that will be refused.
            if (ownerIsStuck) {
                Text(
                    "You created this group. Remove everyone else before you can leave it.",
                    Modifier.fillMaxWidth().padding(horizontal = 20.dp, vertical = 16.dp),
                    style = MaterialTheme.typography.bodySmall,
                    color = colors.onSurfaceVariant,
                )
            } else if (canLeave) {
                Row(
                    Modifier
                        .fillMaxWidth()
                        .clickable { viewModel.leave() }
                        .padding(horizontal = 20.dp, vertical = 16.dp),
                    horizontalArrangement = Arrangement.Start,
                ) {
                    Text(
                        "Leave group",
                        style = MaterialTheme.typography.titleSmall,
                        fontWeight = FontWeight.SemiBold,
                        color = colors.error,
                    )
                }
            }
            Spacer(Modifier.height(32.dp))
        }
    }
}

@Composable
private fun MemberRow(
    member: UiMember,
    trailing: String?,
    trailingColor: androidx.compose.ui.graphics.Color,
    onTrailing: (() -> Unit)?,
) {
    val colors = MaterialTheme.colorScheme
    val dim = if (member.active) 1f else 0.45f
    Row(
        Modifier.fillMaxWidth().padding(horizontal = 20.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.background(androidx.compose.ui.graphics.Color.Transparent)) {
            Avatar(name = member.name, size = 42.dp)
        }
        Spacer(Modifier.width(14.dp))
        Text(
            member.name,
            Modifier.weight(1f),
            style = MaterialTheme.typography.bodyLarge,
            color = colors.onSurface.copy(alpha = dim),
        )
        trailing?.let {
            Text(
                it,
                Modifier
                    .then(if (onTrailing != null) Modifier.clickable(onClick = onTrailing) else Modifier)
                    .padding(6.dp),
                style = MaterialTheme.typography.labelMedium,
                color = trailingColor.copy(alpha = dim),
            )
        }
    }
}
