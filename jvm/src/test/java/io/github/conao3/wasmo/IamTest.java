package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.util.List;
import java.util.Map;
import java.util.stream.Collectors;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.services.iam.IamClient;
import software.amazon.awssdk.services.iam.model.AttachedPolicy;
import software.amazon.awssdk.services.iam.model.EvaluationResult;
import software.amazon.awssdk.services.iam.model.NoSuchEntityException;
import software.amazon.awssdk.services.iam.model.PolicyEvaluationDecisionType;

class IamTest {
    static final String TRUST =
            "{\"Version\":\"2012-10-17\",\"Statement\":[{\"Effect\":\"Allow\",\"Principal\":{\"Service\":\"lambda.amazonaws.com\"},\"Action\":\"sts:AssumeRole\"}]}";
    static final String READ_ORDERS =
            "{\"Version\":\"2012-10-17\",\"Statement\":[{\"Effect\":\"Allow\",\"Action\":\"dynamodb:GetItem\",\"Resource\":\"arn:aws:dynamodb:*:*:table/orders\"}]}";

    @Test
    void roleWithAnAttachedPolicyAndASimulation() {
        try (var mock = Wasmo.create()) {
            var iam = Mocks.client(mock, IamClient.builder());
            var role =
                    iam.createRole(r -> r.roleName("app").assumeRolePolicyDocument(TRUST)).role();
            assertEquals("arn:aws:iam::000000000000:role/app", role.arn());
            var policy =
                    iam.createPolicy(r -> r.policyName("read-orders").policyDocument(READ_ORDERS))
                            .policy();
            iam.attachRolePolicy(r -> r.roleName("app").policyArn(policy.arn()));
            assertEquals(
                    List.of("read-orders"),
                    iam.listAttachedRolePolicies(r -> r.roleName("app")).attachedPolicies().stream()
                            .map(AttachedPolicy::policyName)
                            .toList());

            var results =
                    iam.simulatePrincipalPolicy(
                                    r ->
                                            r.policySourceArn(role.arn())
                                                    .actionNames(
                                                            "dynamodb:GetItem", "dynamodb:PutItem")
                                                    .resourceArns(
                                                            "arn:aws:dynamodb:us-east-1:000000000000:table/orders"))
                            .evaluationResults();
            assertEquals(
                    Map.of(
                            "dynamodb:GetItem", PolicyEvaluationDecisionType.ALLOWED,
                            "dynamodb:PutItem", PolicyEvaluationDecisionType.IMPLICIT_DENY),
                    results.stream()
                            .collect(
                                    Collectors.toMap(
                                            EvaluationResult::evalActionName,
                                            EvaluationResult::evalDecision)));
            assertThrows(
                    NoSuchEntityException.class, () -> iam.getRole(r -> r.roleName("missing")));
        }
    }
}
