from apps.web.llm_credentials import LlmCredentialStore


def test_llm_credentials_are_profile_scoped_and_private(tmp_path):
    store = LlmCredentialStore(tmp_path)
    store.save("planner", "sk-planner")
    store.save("titler", "sk-titler")

    assert store.resolve("planner") == "sk-planner"
    assert store.resolve("titler") == "sk-titler"
    assert store.source("planner") == "saved"
    assert (tmp_path / "_secrets" / "llm_profiles" / "planner" / "API_KEY").stat().st_mode & 0o777 == 0o600

    store.clear("planner")
    assert store.saved_key("planner") == ""
