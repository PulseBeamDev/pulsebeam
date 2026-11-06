from setuptools import Distribution, setup
from wheel.bdist_wheel import bdist_wheel


class NativeDistribution(Distribution):
    def has_ext_modules(self):
        return True


class NativeWheel(bdist_wheel):
    def get_tag(self):
        return "py3", "none", "linux_x86_64"


setup(distclass=NativeDistribution, cmdclass={"bdist_wheel": NativeWheel})
