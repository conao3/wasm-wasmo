using Amazon.IdentityManagement;
using Amazon.IdentityManagement.Model;

namespace Wasmo.Tests;

public class IamTest
{
    private const string Trust = """{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"lambda.amazonaws.com"},"Action":"sts:AssumeRole"}]}""";
    private const string ReadOrders = """{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"dynamodb:GetItem","Resource":"arn:aws:dynamodb:*:*:table/orders"}]}""";

    [Fact]
    public async Task SimulatesARoleWithAnAttachedPolicy()
    {
        using var mock = WasmoMock.Create();
        using var iam = new AmazonIdentityManagementServiceClient(mock.Configure(new AmazonIdentityManagementServiceConfig()));
        var role = (await iam.CreateRoleAsync(new CreateRoleRequest { RoleName = "app", AssumeRolePolicyDocument = Trust })).Role;
        Assert.Equal("arn:aws:iam::000000000000:role/app", role.Arn);
        var policy = (await iam.CreatePolicyAsync(new CreatePolicyRequest { PolicyName = "read-orders", PolicyDocument = ReadOrders })).Policy;
        await iam.AttachRolePolicyAsync(new AttachRolePolicyRequest { RoleName = "app", PolicyArn = policy.Arn });
        var attached = await iam.ListAttachedRolePoliciesAsync(new ListAttachedRolePoliciesRequest { RoleName = "app" });
        Assert.Equal(["read-orders"], attached.AttachedPolicies.Select(p => p.PolicyName));

        var results = (await iam.SimulatePrincipalPolicyAsync(new SimulatePrincipalPolicyRequest
        {
            PolicySourceArn = role.Arn,
            ActionNames = ["dynamodb:GetItem", "dynamodb:PutItem"],
            ResourceArns = ["arn:aws:dynamodb:us-east-1:000000000000:table/orders"],
        })).EvaluationResults;
        Assert.Equal(PolicyEvaluationDecisionType.Allowed, results.Single(r => r.EvalActionName == "dynamodb:GetItem").EvalDecision);
        Assert.Equal(PolicyEvaluationDecisionType.ImplicitDeny, results.Single(r => r.EvalActionName == "dynamodb:PutItem").EvalDecision);
        await Assert.ThrowsAsync<NoSuchEntityException>(() => iam.GetRoleAsync(new GetRoleRequest { RoleName = "missing" }));
    }
}
