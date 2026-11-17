CREATE POLICY command_template_release_validation ON command_templates FOR SELECT USING (
    office_is_releasing_checkout(workspace_id, checkout_id)
);
