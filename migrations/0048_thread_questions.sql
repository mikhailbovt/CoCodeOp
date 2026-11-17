CREATE TABLE thread_questions (
    workspace_id uuid NOT NULL,
    channel_id uuid NOT NULL,
    question_id uuid NOT NULL,
    question_event_id uuid NOT NULL,
    asker_user_id uuid NOT NULL,
    asker_agent_id uuid,
    recipient_user_id uuid NOT NULL,
    recipient_agent_id uuid,
    expires_at timestamptz NOT NULL,
    answer_id uuid,
    answer_event_id uuid,
    PRIMARY KEY (workspace_id, question_id),
    UNIQUE (workspace_id, question_event_id),
    UNIQUE (workspace_id, answer_id),
    UNIQUE (workspace_id, answer_event_id),
    CHECK ((answer_id IS NULL) = (answer_event_id IS NULL)),
    CHECK ((asker_user_id,asker_agent_id) IS DISTINCT FROM (recipient_user_id,recipient_agent_id)),
    FOREIGN KEY (workspace_id, channel_id, question_id) REFERENCES messages (workspace_id, channel_id, id),
    FOREIGN KEY (workspace_id, channel_id, answer_id) REFERENCES messages (workspace_id, channel_id, id),
    FOREIGN KEY (workspace_id, question_event_id) REFERENCES events (workspace_id, id),
    FOREIGN KEY (workspace_id, answer_event_id) REFERENCES events (workspace_id, id),
    FOREIGN KEY (workspace_id, asker_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, asker_agent_id, asker_user_id) REFERENCES agents (workspace_id, id, owner_user_id),
    FOREIGN KEY (workspace_id, recipient_user_id) REFERENCES workspace_memberships (workspace_id, user_id),
    FOREIGN KEY (workspace_id, recipient_agent_id, recipient_user_id) REFERENCES agents (workspace_id, id, owner_user_id)
);

ALTER TABLE thread_questions ENABLE ROW LEVEL SECURITY;
ALTER TABLE thread_questions FORCE ROW LEVEL SECURITY;
CREATE POLICY question_participants ON thread_questions FOR SELECT USING (
    workspace_id=office_workspace_id() AND (
        (asker_user_id,asker_agent_id) IS NOT DISTINCT FROM (office_user_id(),office_agent_id()) OR
        (recipient_user_id,recipient_agent_id) IS NOT DISTINCT FROM (office_user_id(),office_agent_id())
    ) AND EXISTS (SELECT 1 FROM channels c WHERE (c.workspace_id,c.id)=(thread_questions.workspace_id,thread_questions.channel_id))
);
CREATE POLICY question_asker ON thread_questions FOR INSERT WITH CHECK (
    workspace_id=office_workspace_id() AND
    (asker_user_id,asker_agent_id) IS NOT DISTINCT FROM (office_user_id(),office_agent_id())
    AND answer_id IS NULL AND EXISTS (
        SELECT 1 FROM messages m WHERE (m.workspace_id,m.channel_id,m.id)=
            (thread_questions.workspace_id,thread_questions.channel_id,thread_questions.question_id)
        AND (m.author_user_id,m.author_agent_id) IS NOT DISTINCT FROM (office_user_id(),office_agent_id())
    )
);
CREATE POLICY question_recipient_answer ON thread_questions FOR UPDATE USING (
    workspace_id=office_workspace_id() AND
    (recipient_user_id,recipient_agent_id) IS NOT DISTINCT FROM (office_user_id(),office_agent_id())
) WITH CHECK (
    workspace_id=office_workspace_id() AND
    (recipient_user_id,recipient_agent_id) IS NOT DISTINCT FROM (office_user_id(),office_agent_id())
    AND EXISTS (
        SELECT 1 FROM messages m WHERE (m.workspace_id,m.channel_id,m.id,m.reply_to)=
            (thread_questions.workspace_id,thread_questions.channel_id,thread_questions.answer_id,thread_questions.question_id)
        AND (m.author_user_id,m.author_agent_id) IS NOT DISTINCT FROM (office_user_id(),office_agent_id())
    )
);
